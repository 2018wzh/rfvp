//! Standard-library entry point for RFVP's host-neutral core.
//!
//! The hosted surface is intentionally a small layer on top of the upstream
//! portable core.  It does not know about any embedding product, ABI, serializer
//! or native handles.  Its responsibility is to turn one core tick into one
//! bounded, typed delta that an embedding can validate and commit atomically.

use alloc::vec::Vec;

use crate::host_api::{
    AudioParams, AudioStreamDesc, AudioStreamId, ColorRgba, DrawSolidCommand, DrawSpriteCommand,
    EncodedAudioKind, PixelFormat, PlatformCallbacks, RfvpAudio, RfvpError, RfvpEvent,
    RfvpFileSystem, RfvpHost, RfvpLogLevel, RfvpRenderer, RfvpResult, TextureDesc, TextureId,
    TextureRect,
};
pub use crate::no_std_core::{
    RfvpBootConfig as HostedBootConfig, RfvpCoreConfig as HostedConfig,
    RfvpCoreRunState as HostedRunState, RfvpLoadedGame as HostedLoadedGame,
    RfvpResourceEntry as HostedResourceEntry, RfvpTickResult as HostedTickResult, RfvpCore,
};
#[cfg(feature = "hosted")]
pub use crate::no_std_core::{HostedCoreSnapshot as HostedSnapshot, HOSTED_CORE_SNAPSHOT_VERSION};

/// Increment only for a deliberately incompatible hosted-core wire contract.
pub const HOSTED_ABI_VERSION: u16 = 1;

/// Hard caps for one hosted transaction.  Every cap is fail-closed: a core
/// tick that exceeds it returns `CapacityExceeded`, and no partial delta is
/// returned to an embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostedLimits {
    pub max_input_events: usize,
    pub max_scene_operations: usize,
    pub max_texture_bytes: usize,
    pub max_audio_operations: usize,
    pub max_audio_bytes: usize,
}

impl Default for HostedLimits {
    fn default() -> Self {
        Self {
            max_input_events: 256,
            max_scene_operations: 65_536,
            max_texture_bytes: 64 * 1024 * 1024,
            max_audio_operations: 4_096,
            max_audio_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Input accepted by exactly one hosted step.  The embedding owns event
/// collection; RFVP never reaches into a platform event queue.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostedStepInput {
    pub events: Vec<RfvpEvent>,
}

/// Texture payload retained only for the current transaction.  The host must
/// validate dimensions, byte count and resource policy before it allocates or
/// uploads a backend texture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedTextureData {
    pub id: TextureId,
    pub desc: TextureDesc,
    pub pixels: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedTextureUpdate {
    pub id: TextureId,
    pub rect: TextureRect,
    pub pixels: Vec<u8>,
}

/// Semantic scene operations, preserving upstream renderer data rather than
/// producing a software-rasterized frame or a product-specific DTO.
#[derive(Debug, Clone, PartialEq)]
pub enum HostedSceneOperation {
    CreateTexture(HostedTextureData),
    UpdateTexture(HostedTextureUpdate),
    DestroyTexture(TextureId),
    BeginFrame {
        width: u32,
        height: u32,
        clear: Option<ColorRgba>,
    },
    DrawSprite(DrawSpriteCommand),
    DrawSolid(DrawSolidCommand),
    EndFrame,
    Present,
}

/// Semantic audio operations.  Encoded and PCM payloads are bounded and are
/// copied only when the RFVP core actually emits a command.
#[derive(Debug, Clone, PartialEq)]
pub enum HostedAudioOperation {
    LoadEncoded {
        id: AudioStreamId,
        kind: EncodedAudioKind,
        bytes: Vec<u8>,
    },
    CreateStream {
        id: AudioStreamId,
        desc: AudioStreamDesc,
    },
    SubmitI16 {
        id: AudioStreamId,
        samples: Vec<i16>,
    },
    SubmitF32 {
        id: AudioStreamId,
        samples: Vec<f32>,
    },
    Play {
        id: AudioStreamId,
        params: AudioParams,
        fade_in_ms: u32,
    },
    Stop {
        id: AudioStreamId,
        fade_ms: u32,
    },
    Pause(AudioStreamId),
    Resume(AudioStreamId),
    SetParams {
        id: AudioStreamId,
        params: AudioParams,
    },
    SetMasterVolume(f32),
    DestroyStream(AudioStreamId),
    Tick {
        elapsed_us: u64,
    },
}

/// The only presentation and audio result produced by one hosted step.
///
/// An embedding may convert this into its own ABI payload, but must not commit
/// any part of it until all resource, profile and binding checks have passed.
#[derive(Debug, Clone, PartialEq)]
pub struct HostedStepDelta {
    pub tick: HostedTickResult,
    pub scene: Vec<HostedSceneOperation>,
    pub audio: Vec<HostedAudioOperation>,
}

/// A session owns its pending input, core state and capture limits.  It is not
/// `Sync` by construction: concurrent games must use separate sessions and
/// separate hosts rather than a process-wide lock.
pub struct HostedSession {
    core: RfvpCore,
    limits: HostedLimits,
}

impl HostedSession {
    pub fn new(config: HostedConfig, limits: HostedLimits) -> RfvpResult<Self> {
        if limits.max_input_events == 0
            || limits.max_scene_operations == 0
            || limits.max_texture_bytes == 0
            || limits.max_audio_operations == 0
            || limits.max_audio_bytes == 0
        {
            return Err(RfvpError::InvalidArgument);
        }
        Ok(Self {
            core: RfvpCore::new(config),
            limits,
        })
    }

    pub fn core(&self) -> &RfvpCore {
        &self.core
    }

    pub fn limits(&self) -> HostedLimits {
        self.limits
    }

    pub fn snapshot(&self) -> RfvpResult<HostedSnapshot> {
        self.core.capture_hosted_snapshot()
    }

    pub fn restore(&mut self, snapshot: &HostedSnapshot) -> RfvpResult<()> {
        self.core.restore_hosted_snapshot(snapshot)
    }

    /// Boot through the same constrained ports used by `step`.  A boot that
    /// attempts to present or emit audio is rejected rather than silently
    /// dropping output, because there is no commit delta at this lifecycle
    /// boundary.
    pub fn boot<H: RfvpHost>(
        &mut self,
        host: &mut H,
        config: HostedBootConfig<'_>,
    ) -> RfvpResult<()>
    where
        <H::FileSystem as RfvpFileSystem>::File: 'static,
    {
        let mut recording = RecordingHost::new(host, self.limits);
        self.core.boot(&mut recording, config)?;
        recording.finish()?;
        if !recording.renderer.operations.is_empty() || !recording.audio.operations.is_empty() {
            return Err(RfvpError::InvalidData);
        }
        Ok(())
    }

    /// Runs one bounded RFVP tick and captures all host-facing work in one
    /// typed delta.  Errors intentionally prevent a partial delta from
    /// escaping the session.
    pub fn step<H: RfvpHost>(
        &mut self,
        host: &mut H,
        input: HostedStepInput,
    ) -> RfvpResult<HostedStepDelta> {
        if input.events.len() > self.limits.max_input_events {
            return Err(RfvpError::CapacityExceeded);
        }
        for event in input.events {
            self.core.push_event(event)?;
        }

        let mut recording = RecordingHost::new(host, self.limits);
        let tick = self.core.tick(&mut recording)?;
        recording.finish()?;
        Ok(HostedStepDelta {
            tick,
            scene: recording.renderer.operations,
            audio: recording.audio.operations,
        })
    }
}

struct RecordingHost<'a, H: RfvpHost> {
    inner: &'a mut H,
    renderer: RecordingRenderer,
    audio: RecordingAudio,
}

impl<'a, H: RfvpHost> RecordingHost<'a, H> {
    fn new(inner: &'a mut H, limits: HostedLimits) -> Self {
        Self {
            inner,
            renderer: RecordingRenderer::new(limits),
            audio: RecordingAudio::new(limits),
        }
    }

    fn finish(&self) -> RfvpResult<()> {
        self.renderer.finish()?;
        self.audio.finish()
    }
}

impl<H: RfvpHost> RfvpHost for RecordingHost<'_, H> {
    type FileSystem = H::FileSystem;
    type Renderer = RecordingRenderer;
    type Audio = RecordingAudio;
    type Clock = H::Clock;

    fn fs(&mut self) -> &mut Self::FileSystem {
        self.inner.fs()
    }

    fn renderer(&mut self) -> &mut Self::Renderer {
        &mut self.renderer
    }

    fn audio(&mut self) -> &mut Self::Audio {
        &mut self.audio
    }

    fn clock(&mut self) -> &mut Self::Clock {
        self.inner.clock()
    }

    fn log(&mut self, level: RfvpLogLevel, message: &str) {
        self.inner.log(level, message);
    }

    fn platform_callbacks(&mut self) -> PlatformCallbacks {
        self.inner.platform_callbacks()
    }
}

struct RecordingRenderer {
    limits: HostedLimits,
    texture_bytes: usize,
    operations: Vec<HostedSceneOperation>,
    textures: Vec<(TextureId, TextureDesc)>,
    failure: Option<RfvpError>,
}

impl RecordingRenderer {
    fn new(limits: HostedLimits) -> Self {
        Self {
            limits,
            texture_bytes: 0,
            operations: Vec::new(),
            textures: Vec::new(),
            failure: None,
        }
    }

    fn push(&mut self, operation: HostedSceneOperation) -> RfvpResult<()> {
        self.finish()?;
        if self.operations.len() >= self.limits.max_scene_operations {
            return Err(RfvpError::CapacityExceeded);
        }
        self.operations.push(operation);
        Ok(())
    }

    fn finish(&self) -> RfvpResult<()> {
        self.failure.map_or(Ok(()), Err)
    }

    fn reserve_texture_bytes(&mut self, additional: usize) -> RfvpResult<()> {
        self.finish()?;
        self.texture_bytes = self
            .texture_bytes
            .checked_add(additional)
            .ok_or(RfvpError::CapacityExceeded)?;
        if self.texture_bytes > self.limits.max_texture_bytes {
            return Err(RfvpError::CapacityExceeded);
        }
        Ok(())
    }
}

impl RfvpRenderer for RecordingRenderer {
    fn create_texture(
        &mut self,
        id: TextureId,
        desc: TextureDesc,
        pixels: Option<&[u8]>,
    ) -> RfvpResult<()> {
        self.finish()?;
        validate_texture_desc(desc)?;
        let pixels = pixels.map(|pixels| {
            validate_texture_pixels(desc, pixels.len())?;
            self.reserve_texture_bytes(pixels.len())?;
            Ok::<Vec<u8>, RfvpError>(pixels.to_vec())
        }).transpose()?;
        if let Some((_, previous)) = self.textures.iter().find(|(known, _)| *known == id) {
            if *previous != desc {
                return Err(RfvpError::InvalidData);
            }
            let pixels = pixels.ok_or(RfvpError::InvalidData)?;
            return self.push(HostedSceneOperation::UpdateTexture(HostedTextureUpdate {
                id,
                rect: TextureRect {
                    x: 0,
                    y: 0,
                    width: desc.width,
                    height: desc.height,
                },
                pixels,
            }));
        }
        self.textures.push((id, desc));
        self.push(HostedSceneOperation::CreateTexture(HostedTextureData { id, desc, pixels }))
    }

    fn update_texture(&mut self, id: TextureId, rect: TextureRect, pixels: &[u8]) -> RfvpResult<()> {
        self.finish()?;
        let Some((_, desc)) = self.textures.iter().find(|(known, _)| *known == id) else {
            return Err(RfvpError::NotFound);
        };
        if rect.width == 0
            || rect.height == 0
            || rect.x.checked_add(rect.width).ok_or(RfvpError::CapacityExceeded)? > desc.width
            || rect.y.checked_add(rect.height).ok_or(RfvpError::CapacityExceeded)? > desc.height
        {
            return Err(RfvpError::InvalidArgument);
        }
        validate_texture_region(*desc, rect, pixels.len())?;
        self.reserve_texture_bytes(pixels.len())?;
        self.push(HostedSceneOperation::UpdateTexture(HostedTextureUpdate {
            id,
            rect,
            pixels: pixels.to_vec(),
        }))
    }

    fn destroy_texture(&mut self, id: TextureId) {
        if self.failure.is_some() {
            return;
        }
        if !self.textures.iter().any(|(known, _)| *known == id) {
            self.failure = Some(RfvpError::NotFound);
            return;
        }
        if let Err(error) = self.push(HostedSceneOperation::DestroyTexture(id)) {
            self.failure = Some(error);
            return;
        }
        self.textures.retain(|(known, _)| *known != id);
    }

    fn begin_frame(&mut self, width: u32, height: u32, clear: Option<ColorRgba>) -> RfvpResult<()> {
        if width == 0 || height == 0 {
            return Err(RfvpError::InvalidArgument);
        }
        self.push(HostedSceneOperation::BeginFrame { width, height, clear })
    }

    fn draw_sprite(&mut self, command: &DrawSpriteCommand) -> RfvpResult<()> {
        self.push(HostedSceneOperation::DrawSprite(*command))
    }

    fn draw_solid(&mut self, command: &DrawSolidCommand) -> RfvpResult<()> {
        self.push(HostedSceneOperation::DrawSolid(*command))
    }

    fn end_frame(&mut self) -> RfvpResult<()> {
        self.push(HostedSceneOperation::EndFrame)
    }

    fn present(&mut self) -> RfvpResult<()> {
        self.push(HostedSceneOperation::Present)
    }
}

fn validate_texture_desc(desc: TextureDesc) -> RfvpResult<()> {
    if desc.width == 0 || desc.height == 0 || desc.mip_count == 0 {
        return Err(RfvpError::InvalidArgument);
    }
    match desc.format {
        PixelFormat::Rgba8
        | PixelFormat::Bgra8
        | PixelFormat::Rgb8
        | PixelFormat::Luma8
        | PixelFormat::LumaA8
        | PixelFormat::Alpha8 => Ok(()),
    }
}

fn texture_bytes_per_pixel(format: PixelFormat) -> usize {
    match format {
        PixelFormat::Rgba8 | PixelFormat::Bgra8 => 4,
        PixelFormat::Rgb8 => 3,
        PixelFormat::LumaA8 => 2,
        PixelFormat::Luma8 | PixelFormat::Alpha8 => 1,
    }
}

fn validate_texture_pixels(desc: TextureDesc, bytes: usize) -> RfvpResult<()> {
    let expected = (desc.width as usize)
        .checked_mul(desc.height as usize)
        .and_then(|pixels| pixels.checked_mul(texture_bytes_per_pixel(desc.format)))
        .ok_or(RfvpError::CapacityExceeded)?;
    if bytes != expected {
        return Err(RfvpError::InvalidData);
    }
    Ok(())
}

fn validate_texture_region(desc: TextureDesc, rect: TextureRect, bytes: usize) -> RfvpResult<()> {
    validate_texture_pixels(
        TextureDesc {
            width: rect.width,
            height: rect.height,
            format: desc.format,
            mip_count: 1,
        },
        bytes,
    )
}

struct RecordingAudio {
    limits: HostedLimits,
    audio_bytes: usize,
    operations: Vec<HostedAudioOperation>,
    failure: Option<RfvpError>,
}

impl RecordingAudio {
    fn new(limits: HostedLimits) -> Self {
        Self {
            limits,
            audio_bytes: 0,
            operations: Vec::new(),
            failure: None,
        }
    }

    fn push(&mut self, operation: HostedAudioOperation) -> RfvpResult<()> {
        self.finish()?;
        if self.operations.len() >= self.limits.max_audio_operations {
            return Err(RfvpError::CapacityExceeded);
        }
        self.operations.push(operation);
        Ok(())
    }

    fn finish(&self) -> RfvpResult<()> {
        self.failure.map_or(Ok(()), Err)
    }

    fn reserve_audio_bytes(&mut self, additional: usize) -> RfvpResult<()> {
        self.finish()?;
        self.audio_bytes = self
            .audio_bytes
            .checked_add(additional)
            .ok_or(RfvpError::CapacityExceeded)?;
        if self.audio_bytes > self.limits.max_audio_bytes {
            return Err(RfvpError::CapacityExceeded);
        }
        Ok(())
    }
}

impl RfvpAudio for RecordingAudio {
    fn load_encoded(&mut self, id: AudioStreamId, kind: EncodedAudioKind, bytes: &[u8]) -> RfvpResult<()> {
        self.reserve_audio_bytes(bytes.len())?;
        self.push(HostedAudioOperation::LoadEncoded { id, kind, bytes: bytes.to_vec() })
    }

    fn create_stream(&mut self, id: AudioStreamId, desc: AudioStreamDesc) -> RfvpResult<()> {
        self.push(HostedAudioOperation::CreateStream { id, desc })
    }

    fn submit_i16(&mut self, id: AudioStreamId, samples: &[i16]) -> RfvpResult<()> {
        let bytes = samples.len().checked_mul(core::mem::size_of::<i16>()).ok_or(RfvpError::CapacityExceeded)?;
        self.reserve_audio_bytes(bytes)?;
        self.push(HostedAudioOperation::SubmitI16 { id, samples: samples.to_vec() })
    }

    fn submit_f32(&mut self, id: AudioStreamId, samples: &[f32]) -> RfvpResult<()> {
        let bytes = samples.len().checked_mul(core::mem::size_of::<f32>()).ok_or(RfvpError::CapacityExceeded)?;
        self.reserve_audio_bytes(bytes)?;
        self.push(HostedAudioOperation::SubmitF32 { id, samples: samples.to_vec() })
    }

    fn play(&mut self, id: AudioStreamId, params: AudioParams, fade_in_ms: u32) -> RfvpResult<()> {
        self.push(HostedAudioOperation::Play { id, params, fade_in_ms })
    }

    fn stop(&mut self, id: AudioStreamId, fade_ms: u32) -> RfvpResult<()> {
        self.push(HostedAudioOperation::Stop { id, fade_ms })
    }

    fn pause(&mut self, id: AudioStreamId) -> RfvpResult<()> {
        self.push(HostedAudioOperation::Pause(id))
    }

    fn resume(&mut self, id: AudioStreamId) -> RfvpResult<()> {
        self.push(HostedAudioOperation::Resume(id))
    }

    fn set_params(&mut self, id: AudioStreamId, params: AudioParams) -> RfvpResult<()> {
        self.push(HostedAudioOperation::SetParams { id, params })
    }

    fn set_master_volume(&mut self, volume: f32) -> RfvpResult<()> {
        if !volume.is_finite() {
            return Err(RfvpError::InvalidArgument);
        }
        self.push(HostedAudioOperation::SetMasterVolume(volume))
    }

    fn destroy_stream(&mut self, id: AudioStreamId) {
        if self.failure.is_some() {
            return;
        }
        if let Err(error) = self.push(HostedAudioOperation::DestroyStream(id)) {
            self.failure = Some(error);
        }
    }

    fn tick(&mut self, elapsed_us: u64) -> RfvpResult<()> {
        self.push(HostedAudioOperation::Tick { elapsed_us })
    }
}
