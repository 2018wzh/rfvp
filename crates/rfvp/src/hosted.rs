//! Standard-library entry point for RFVP's host-neutral core.
//!
//! The hosted surface is intentionally a small layer on top of the upstream
//! portable core. It does not know about any embedding product, ABI, serializer
//! or native handles. Its responsibility is to run one core tick and expose
//! only the audio/video diagnostics and damage metadata needed by the provider.

use alloc::vec::Vec;

use crate::host_api::{
    AudioParams, AudioStreamDesc, AudioStreamId, ColorRgba, DrawSolidCommand, DrawSpriteCommand,
    EncodedAudioKind, PixelBuffer, PixelFormat, PlatformCallbacks, RectI32, RfvpAudio, RfvpError,
    RfvpEvent, RfvpFile, RfvpFileSystem, RfvpHost, RfvpLogLevel, RfvpRenderer, RfvpResult,
    TextureDesc, TextureId, TextureRect,
};
pub use crate::no_std_core::{
    RfvpBootConfig as HostedBootConfig, RfvpCore, RfvpCoreConfig as HostedConfig,
    RfvpCoreRunState as HostedRunState, RfvpLoadedGame as HostedLoadedGame,
    RfvpResourceEntry as HostedResourceEntry, RfvpTickResult as HostedTickResult,
};
#[cfg(feature = "hosted")]
pub use crate::vm_runner::HostedVmTraceRecord;

/// Increment only for a deliberately incompatible hosted-core wire contract.
pub const HOSTED_ABI_VERSION: u16 = 3;

/// Hard caps for one hosted transaction.  Every cap is fail-closed: a core
/// tick that exceeds it returns `CapacityExceeded`, and no partial delta is
/// returned to an embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostedLimits {
    pub max_input_events: usize,
    /// Bounded structured hosted-core diagnostics retained in an Evidence
    /// transaction.  Shipping never records these entries.
    pub max_log_records: usize,
    /// Accounting bound for structured diagnostic records.  Hosted records do
    /// not contain host paths, payload bytes, or formatted engine messages.
    pub max_log_bytes: usize,
}

impl Default for HostedLimits {
    fn default() -> Self {
        Self {
            max_input_events: 256,
            max_log_records: 256,
            max_log_bytes: 16 * 1024,
        }
    }
}

/// Input accepted by exactly one hosted step.  The embedding owns event
/// collection; RFVP never reaches into a platform event queue.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostedStepInput {
    pub events: Vec<RfvpEvent>,
}

/// Semantic audio operations.  Encoded and PCM payloads are bounded and are
/// copied only when the RFVP core actually emits a command.
#[derive(Debug, Clone, PartialEq)]
pub enum HostedAudioOperation {
    LoadResource {
        id: AudioStreamId,
        kind: EncodedAudioKind,
        resource_uri: String,
    },
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostedVideoOperation {
    Play {
        resource_uri: String,
        byte_len: u64,
        modal_with_audio: bool,
        stage_width: u32,
        stage_height: u32,
    },
}

/// Text emitted by a script print operation. The provider consumes these
/// events synchronously for its Hook call before it acquires a surface; they
/// are never part of the live ABI delta or persisted state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedTextEvent {
    pub slot: u8,
    pub text: String,
}

/// Stable hosted-core diagnostic identity.  It deliberately carries no
/// formatted RFVP text so an embedding cannot accidentally persist a local
/// path, game payload, or platform handle in its observability pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostedLogEvent {
    QuitRequested,
    CoreFailure,
    ResourceWarning,
    HostMessage,
}

impl HostedLogEvent {
    pub const fn code(self) -> &'static str {
        match self {
            Self::QuitRequested => "rfvp.host.quit_requested",
            Self::CoreFailure => "rfvp.core.failure",
            Self::ResourceWarning => "rfvp.resource.warning",
            Self::HostMessage => "rfvp.host.message",
        }
    }
}

/// A bounded diagnostic emitted by the hosted-core boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostedLogRecord {
    pub level: RfvpLogLevel,
    pub event: HostedLogEvent,
}

/// The only presentation and audio result produced by one hosted step.
///
/// An embedding may convert this into its own ABI payload, but must not commit
/// any part of it until all resource, profile and binding checks have passed.
#[derive(Debug)]
pub struct HostedStepDelta {
    pub tick: HostedTickResult,
    pub audio: Vec<HostedAudioOperation>,
    pub video: Vec<HostedVideoOperation>,
    pub logs: Vec<HostedLogRecord>,
    pub log_dropped_count: u32,
    /// True when the direct Astra presentation state differs from the last
    /// presented frame. This is derived from typed renderer state, never a
    /// pixel or content hash.
    pub visual_changed: bool,
    /// Conservative pixel-space damage derived from typed draw commands and
    /// texture identities. It never scans or hashes framebuffer bytes.
    pub visual_damage: HostedVisualDamage,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HostedVisualDamage {
    #[default]
    Unchanged,
    Full,
    Rect(HostedDamageRect),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostedDamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The production profile leaves instruction evidence disabled. Evidence is
/// opt-in and bounded to a fixed crash ring; it never formats or serializes an
/// opcode on the dispatch path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostedTraceProfile {
    #[default]
    Shipping,
    Evidence {
        crash_trace_capacity: usize,
    },
}

/// A session owns its pending input, core state and capture limits.  It is not
/// `Sync` by construction: concurrent games must use separate sessions and
/// separate hosts rather than a process-wide lock.
pub struct HostedSession {
    core: RfvpCore,
    limits: HostedLimits,
    // Damage tracker identity must outlive one step. The core legitimately
    // reuses texture ids across ticks, so the tracker must retain resource
    // generations without retaining pixel payloads.
    renderer: DirectSurfaceTracker,
    direct_renderer: Option<crate::soft_render::SoftRenderer>,
    capture_logs: bool,
}

impl HostedSession {
    pub fn new(config: HostedConfig, limits: HostedLimits) -> RfvpResult<Self> {
        if limits.max_input_events == 0 || limits.max_log_records == 0 || limits.max_log_bytes == 0
        {
            return Err(RfvpError::InvalidArgument);
        }
        let core = RfvpCore::new(config);
        Ok(Self {
            core,
            limits,
            renderer: DirectSurfaceTracker::new(),
            direct_renderer: None,
            capture_logs: false,
        })
    }

    /// Applies a successful synchronous Hook result before the presentation
    /// surface is acquired.
    pub fn replace_text(&mut self, slot: u8, text: &str) -> RfvpResult<()> {
        self.core.replace_hosted_text(slot, text)
    }

    /// Moves a Host lease allocation into RFVP, rasterizes in place, and returns that allocation.
    pub fn render_direct_surface(
        &mut self,
        width: u32,
        height: u32,
        format: crate::soft_render::PixelFormat,
        pixels: astra_byte_source::OwnedWritableByteBuffer,
    ) -> RfvpResult<astra_byte_source::OwnedWritableByteBuffer> {
        let renderer = match self.direct_renderer.as_mut() {
            Some(renderer) => renderer,
            None => self.direct_renderer.insert(
                crate::soft_render::SoftRenderer::new(width, height, format)
                    .map_err(|_| RfvpError::CapacityExceeded)?,
            ),
        };
        renderer
            .replace_astra_surface(width, height, format, pixels)
            .map_err(|_| RfvpError::InvalidData)?;
        self.core.render_hosted_software(renderer)?;
        renderer
            .take_astra_surface()
            .map_err(|_| RfvpError::InvalidData)
    }

    pub fn core(&self) -> &RfvpCore {
        &self.core
    }

    pub fn set_trace_profile(&mut self, profile: HostedTraceProfile) -> RfvpResult<()> {
        let capacity = match profile {
            HostedTraceProfile::Shipping => 0,
            HostedTraceProfile::Evidence {
                crash_trace_capacity,
            } if crash_trace_capacity > 0 => crash_trace_capacity,
            HostedTraceProfile::Evidence { .. } => return Err(RfvpError::InvalidArgument),
        };
        self.core.set_hosted_trace_capacity(capacity)?;
        self.capture_logs = matches!(profile, HostedTraceProfile::Evidence { .. });
        Ok(())
    }

    pub fn crash_trace(&self) -> RfvpResult<Vec<HostedVmTraceRecord>> {
        self.core.hosted_trace()
    }

    pub fn quit_requested(&self) -> bool {
        self.core.quit_requested()
    }

    /// Returns the terminal state for the hosted lifecycle, including normal
    /// script completion rather than only an explicit host quit event.
    pub fn is_terminal(&self) -> bool {
        self.core.hosted_terminal()
    }

    /// Acknowledge completion of the one outstanding hosted video operation.
    /// The embedding owns request-token matching; this method deliberately
    /// carries no product-specific identifier.
    pub fn complete_video(&mut self) -> RfvpResult<()> {
        self.core.complete_hosted_video()
    }

    /// Resolves a resource through the embedding-owned host port. The RFVP
    /// core never accesses an ambient or process-owned filesystem.
    pub fn read_resource<H: RfvpHost>(
        &mut self,
        host: &mut H,
        resource_uri: &str,
        max_bytes: usize,
    ) -> RfvpResult<Vec<u8>> {
        if resource_uri.is_empty() || resource_uri.contains('\0') || max_bytes == 0 {
            return Err(RfvpError::InvalidArgument);
        }
        match host.fs().open(resource_uri) {
            Ok(mut file) => file.read_to_vec(max_bytes),
            Err(RfvpError::NotFound) => self.core.read_hosted_resource(resource_uri, max_bytes),
            Err(error) => Err(error),
        }
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
        let mut recording = HostedStepHost::new(host, self.limits, self.capture_logs);
        self.core.boot(&mut recording, config)?;
        recording.finish()?;
        if !recording.renderer.is_empty() || !recording.audio.operations.is_empty() {
            return Err(RfvpError::InvalidData);
        }
        self.core.invalidate_host_render_cache();
        self.renderer.reset();
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

        let renderer = std::mem::replace(&mut self.renderer, DirectSurfaceTracker::new());
        let mut recording =
            HostedStepHost::with_renderer(host, renderer, self.limits, self.capture_logs);
        let tick_result = self.core.tick(&mut recording);
        let finish_result = tick_result.and_then(|tick| recording.finish().map(|()| tick));
        self.renderer = recording.renderer;
        let tick = finish_result?;
        let logs = recording.logs.take();
        let visual_changed = self.renderer.take_visual_changed();
        let visual_damage = self.renderer.take_visual_damage();
        Ok(HostedStepDelta {
            tick,
            audio: recording.audio.operations,
            video: self
                .core
                .take_hosted_video_commands()
                .into_iter()
                .map(|command| HostedVideoOperation::Play {
                    resource_uri: command.resource_uri,
                    byte_len: command.byte_len,
                    modal_with_audio: matches!(
                        command.mode,
                        crate::subsystem::resources::videoplayer::MovieMode::ModalWithAudio
                    ),
                    stage_width: command.screen_w,
                    stage_height: command.screen_h,
                })
                .collect(),
            log_dropped_count: recording.logs.dropped_count,
            logs,
            visual_changed,
            visual_damage,
        })
    }

    /// Drains text events produced by the last successful step. The provider
    /// invokes its synchronous Hook from this method before acquiring a
    /// surface, then the event storage is discarded.
    pub fn take_text_events(&mut self) -> Vec<HostedTextEvent> {
        self.core
            .take_hosted_text_events()
            .into_iter()
            .map(|event| HostedTextEvent {
                slot: event.slot,
                text: event.text,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_profile_requires_a_fixed_positive_ring_capacity() {
        let mut session = HostedSession::new(HostedConfig::default(), HostedLimits::default())
            .expect("default hosted session is valid");
        assert_eq!(
            session.set_trace_profile(HostedTraceProfile::Evidence {
                crash_trace_capacity: 0,
            }),
            Err(RfvpError::InvalidArgument)
        );
        session
            .set_trace_profile(HostedTraceProfile::Evidence {
                crash_trace_capacity: 32,
            })
            .expect("bounded evidence profile is valid");
        session
            .set_trace_profile(HostedTraceProfile::Shipping)
            .expect("shipping profile disables trace capture");
    }

    #[test]
    fn recording_audio_preserves_resource_identity_without_payload_bytes() {
        let mut audio = RecordingAudio::new();
        audio
            .load_resource(AudioStreamId(3), EncodedAudioKind::Ogg, "audio/theme.ogg")
            .expect("resource audio is accepted");
        assert!(matches!(
            audio.operations.as_slice(),
            [HostedAudioOperation::LoadResource { resource_uri, .. }]
                if resource_uri == "audio/theme.ogg"
        ));
    }

    #[test]
    fn direct_surface_tracker_validates_texture_lifetime_without_retaining_pixels() {
        let mut renderer = DirectSurfaceTracker::new();
        let desc = TextureDesc {
            width: 1,
            height: 1,
            format: PixelFormat::Rgba8,
            mip_count: 1,
        };
        let pixels = [1, 2, 3, 4];

        renderer
            .create_texture(TextureId(7), desc, Some(PixelBuffer::Borrowed(&pixels)))
            .expect("the first texture write is accepted");
        assert_eq!(renderer.textures, vec![(TextureId(7), desc)]);
        assert!(renderer.direct_frame.is_empty());
        renderer
            .create_texture(TextureId(7), desc, Some(PixelBuffer::Borrowed(&pixels)))
            .expect("a same-id texture write is an update");
        assert_eq!(renderer.textures, vec![(TextureId(7), desc)]);
    }

    #[test]
    fn shipping_log_recorder_does_not_allocate_records() {
        let mut logs = RecordingLogs::new(HostedLimits::default(), false);
        logs.record(RfvpLogLevel::Error, "core failure");
        assert!(logs.take().is_empty());
        assert_eq!(logs.dropped_count, 0);
    }

    #[test]
    fn evidence_log_recorder_keeps_only_structured_identity() {
        let mut logs = RecordingLogs::new(HostedLimits::default(), true);
        logs.record(RfvpLogLevel::Info, "a local path must not escape");
        assert_eq!(
            logs.take(),
            vec![HostedLogRecord {
                level: RfvpLogLevel::Info,
                event: HostedLogEvent::HostMessage,
            }]
        );
    }

    #[test]
    fn evidence_log_overflow_is_reported_without_blocking_the_step() {
        let limits = HostedLimits {
            max_log_records: 1,
            max_log_bytes: RecordingLogs::ACCOUNTED_BYTES,
            ..HostedLimits::default()
        };
        let mut logs = RecordingLogs::new(limits, true);
        logs.record(RfvpLogLevel::Info, "first");
        logs.record(RfvpLogLevel::Info, "second");
        assert_eq!(logs.take().len(), 1);
        assert_eq!(logs.dropped_count, 1);
    }

    #[test]
    fn direct_frame_damage_is_unchanged_without_semantic_changes() {
        let frame = solid_frame(RectI32 {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        });
        assert_eq!(
            direct_frame_damage(&frame, &frame, &[]),
            HostedVisualDamage::Unchanged
        );
    }

    #[test]
    fn direct_frame_damage_unions_old_and_new_draw_bounds() {
        let previous = solid_frame(RectI32 {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        });
        let current = solid_frame(RectI32 {
            x: 25,
            y: 30,
            width: 30,
            height: 10,
        });
        assert_eq!(
            direct_frame_damage(&previous, &current, &[]),
            HostedVisualDamage::Rect(HostedDamageRect {
                x: 10,
                y: 20,
                width: 45,
                height: 40,
            })
        );
    }

    fn solid_frame(rect: RectI32) -> Vec<DirectFrameCommand> {
        vec![
            DirectFrameCommand::BeginFrame {
                width: 100,
                height: 100,
                clear: Some(ColorRgba::BLACK),
            },
            DirectFrameCommand::DrawSolid(DrawSolidCommand {
                rect,
                color: ColorRgba {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
                blend: crate::host_api::BlendMode::Opaque,
                scissor: None,
            }),
            DirectFrameCommand::EndFrame,
            DirectFrameCommand::Present,
        ]
    }
}

struct HostedStepHost<'a, H: RfvpHost> {
    inner: &'a mut H,
    renderer: DirectSurfaceTracker,
    audio: RecordingAudio,
    logs: RecordingLogs,
}

impl<'a, H: RfvpHost> HostedStepHost<'a, H> {
    fn new(inner: &'a mut H, limits: HostedLimits, capture_logs: bool) -> Self {
        Self {
            inner,
            renderer: DirectSurfaceTracker::new(),
            audio: RecordingAudio::new(),
            logs: RecordingLogs::new(limits, capture_logs),
        }
    }

    fn with_renderer(
        inner: &'a mut H,
        renderer: DirectSurfaceTracker,
        limits: HostedLimits,
        capture_logs: bool,
    ) -> Self {
        Self {
            inner,
            renderer,
            audio: RecordingAudio::new(),
            logs: RecordingLogs::new(limits, capture_logs),
        }
    }

    fn finish(&self) -> RfvpResult<()> {
        self.renderer.finish()?;
        self.audio.finish()
    }
}

impl<H: RfvpHost> RfvpHost for HostedStepHost<'_, H> {
    type FileSystem = H::FileSystem;
    type Renderer = DirectSurfaceTracker;
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
        self.logs.record(level, message);
    }

    fn platform_callbacks(&mut self) -> PlatformCallbacks {
        self.inner.platform_callbacks()
    }
}

struct RecordingLogs {
    limits: HostedLimits,
    enabled: bool,
    bytes: usize,
    records: Vec<HostedLogRecord>,
    dropped_count: u32,
}

impl RecordingLogs {
    const ACCOUNTED_BYTES: usize = 16;

    fn new(limits: HostedLimits, enabled: bool) -> Self {
        Self {
            limits,
            enabled,
            bytes: 0,
            records: Vec::new(),
            dropped_count: 0,
        }
    }

    fn record(&mut self, level: RfvpLogLevel, message: &str) {
        if !self.enabled {
            return;
        }
        let event = classify_hosted_log(level, message);
        let next_bytes = self.bytes.saturating_add(Self::ACCOUNTED_BYTES);
        if self.records.len() >= self.limits.max_log_records
            || next_bytes > self.limits.max_log_bytes
        {
            self.dropped_count = self.dropped_count.saturating_add(1);
            return;
        }
        self.bytes = next_bytes;
        self.records.push(HostedLogRecord { level, event });
    }

    fn take(&mut self) -> Vec<HostedLogRecord> {
        std::mem::take(&mut self.records)
    }
}

fn classify_hosted_log(level: RfvpLogLevel, message: &str) -> HostedLogEvent {
    if message == "quit requested by host event" {
        HostedLogEvent::QuitRequested
    } else if matches!(level, RfvpLogLevel::Error) {
        HostedLogEvent::CoreFailure
    } else if matches!(level, RfvpLogLevel::Warn) {
        HostedLogEvent::ResourceWarning
    } else {
        HostedLogEvent::HostMessage
    }
}

#[derive(Debug, Clone, PartialEq)]
enum DirectFrameCommand {
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

struct DirectSurfaceTracker {
    textures: Vec<(TextureId, TextureDesc)>,
    failure: Option<RfvpError>,
    direct_frame: Vec<DirectFrameCommand>,
    previous_direct_frame: Vec<DirectFrameCommand>,
    direct_resource_changed: bool,
    direct_changed_resources: Vec<TextureId>,
    visual_changed: bool,
    visual_damage: HostedVisualDamage,
}

impl DirectSurfaceTracker {
    fn new() -> Self {
        Self {
            textures: Vec::new(),
            failure: None,
            direct_frame: Vec::new(),
            previous_direct_frame: Vec::new(),
            direct_resource_changed: true,
            direct_changed_resources: Vec::new(),
            visual_changed: true,
            visual_damage: HostedVisualDamage::Full,
        }
    }

    fn finish(&self) -> RfvpResult<()> {
        self.failure.map_or(Ok(()), Err)
    }

    fn is_empty(&self) -> bool {
        self.direct_frame.is_empty() && self.textures.is_empty()
    }

    fn take_visual_changed(&mut self) -> bool {
        core::mem::take(&mut self.visual_changed)
    }

    fn take_visual_damage(&mut self) -> HostedVisualDamage {
        core::mem::take(&mut self.visual_damage)
    }

    fn push_direct(&mut self, command: DirectFrameCommand) -> RfvpResult<()> {
        self.finish()?;
        self.direct_frame.push(command);
        Ok(())
    }

    fn finish_direct_frame(&mut self) {
        let changed = self.direct_resource_changed
            || self.direct_frame.as_slice() != self.previous_direct_frame.as_slice();
        self.visual_damage = direct_frame_damage(
            &self.previous_direct_frame,
            &self.direct_frame,
            &self.direct_changed_resources,
        );
        core::mem::swap(&mut self.direct_frame, &mut self.previous_direct_frame);
        self.direct_frame.clear();
        self.direct_resource_changed = false;
        self.direct_changed_resources.clear();
        self.visual_changed = changed;
    }

    fn reset(&mut self) {
        self.textures.clear();
        self.failure = None;
        self.direct_frame.clear();
        self.previous_direct_frame.clear();
        self.direct_resource_changed = true;
        self.direct_changed_resources.clear();
        self.visual_changed = true;
        self.visual_damage = HostedVisualDamage::Full;
    }
}

impl RfvpRenderer for DirectSurfaceTracker {
    fn create_texture(
        &mut self,
        id: TextureId,
        desc: TextureDesc,
        pixels: Option<PixelBuffer<'_>>,
    ) -> RfvpResult<()> {
        self.finish()?;
        validate_texture_desc(desc)?;
        if let Some(pixels) = pixels.as_ref() {
            validate_texture_pixels(desc, pixels.len())?;
        }
        if let Some((_, known_desc)) = self.textures.iter_mut().find(|(known, _)| *known == id) {
            *known_desc = desc;
        } else {
            self.textures.push((id, desc));
        }
        self.direct_resource_changed = true;
        remember_texture(&mut self.direct_changed_resources, id);
        Ok(())
    }

    fn update_texture(
        &mut self,
        id: TextureId,
        rect: TextureRect,
        pixels: PixelBuffer<'_>,
    ) -> RfvpResult<()> {
        self.finish()?;
        let Some((_, desc)) = self.textures.iter().find(|(known, _)| *known == id) else {
            return Err(RfvpError::NotFound);
        };
        let desc = *desc;
        if rect.width == 0
            || rect.height == 0
            || rect
                .x
                .checked_add(rect.width)
                .ok_or(RfvpError::CapacityExceeded)?
                > desc.width
            || rect
                .y
                .checked_add(rect.height)
                .ok_or(RfvpError::CapacityExceeded)?
                > desc.height
        {
            return Err(RfvpError::InvalidArgument);
        }
        validate_texture_region(desc, rect, pixels.len())?;
        self.direct_resource_changed = true;
        remember_texture(&mut self.direct_changed_resources, id);
        Ok(())
    }

    fn destroy_texture(&mut self, id: TextureId) {
        if self.failure.is_some() {
            return;
        }
        if !self.textures.iter().any(|(known, _)| *known == id) {
            self.failure = Some(RfvpError::NotFound);
            return;
        }
        self.textures.retain(|(known, _)| *known != id);
        self.direct_resource_changed = true;
        remember_texture(&mut self.direct_changed_resources, id);
    }

    fn begin_frame(&mut self, width: u32, height: u32, clear: Option<ColorRgba>) -> RfvpResult<()> {
        if width == 0 || height == 0 {
            return Err(RfvpError::InvalidArgument);
        }
        self.push_direct(DirectFrameCommand::BeginFrame {
            width,
            height,
            clear,
        })
    }

    fn draw_sprite(&mut self, command: &DrawSpriteCommand) -> RfvpResult<()> {
        self.push_direct(DirectFrameCommand::DrawSprite(*command))
    }

    fn draw_solid(&mut self, command: &DrawSolidCommand) -> RfvpResult<()> {
        self.push_direct(DirectFrameCommand::DrawSolid(*command))
    }

    fn end_frame(&mut self) -> RfvpResult<()> {
        self.push_direct(DirectFrameCommand::EndFrame)
    }

    fn present(&mut self) -> RfvpResult<()> {
        self.push_direct(DirectFrameCommand::Present)?;
        self.finish_direct_frame();
        Ok(())
    }
}

fn remember_texture(changed: &mut Vec<TextureId>, id: TextureId) {
    if !changed.contains(&id) {
        changed.push(id);
    }
}

fn direct_frame_damage(
    previous: &[DirectFrameCommand],
    current: &[DirectFrameCommand],
    changed_resources: &[TextureId],
) -> HostedVisualDamage {
    let Some((width, height, clear)) = direct_frame_header(current) else {
        return HostedVisualDamage::Full;
    };
    let Some((previous_width, previous_height, previous_clear)) = direct_frame_header(previous)
    else {
        return HostedVisualDamage::Full;
    };
    if width != previous_width || height != previous_height || clear != previous_clear {
        return HostedVisualDamage::Full;
    }

    let mut prefix = 0usize;
    while prefix < previous.len() && prefix < current.len() && previous[prefix] == current[prefix] {
        prefix += 1;
    }
    let mut previous_suffix = previous.len();
    let mut current_suffix = current.len();
    while previous_suffix > prefix
        && current_suffix > prefix
        && previous[previous_suffix - 1] == current[current_suffix - 1]
    {
        previous_suffix -= 1;
        current_suffix -= 1;
    }

    let mut bounds = None;
    for command in previous[prefix..previous_suffix]
        .iter()
        .chain(current[prefix..current_suffix].iter())
        .chain(previous.iter().filter(|command| {
            command_texture(command).is_some_and(|id| changed_resources.contains(&id))
        }))
        .chain(current.iter().filter(|command| {
            command_texture(command).is_some_and(|id| changed_resources.contains(&id))
        }))
    {
        let Some(rect) = command_bounds(command, width, height) else {
            continue;
        };
        bounds = Some(union_rect(bounds, rect));
    }

    match bounds {
        None => HostedVisualDamage::Unchanged,
        Some(rect)
            if rect.x == 0 && rect.y == 0 && rect.width == width && rect.height == height =>
        {
            HostedVisualDamage::Full
        }
        Some(rect) => HostedVisualDamage::Rect(rect),
    }
}

fn direct_frame_header(frame: &[DirectFrameCommand]) -> Option<(u32, u32, Option<ColorRgba>)> {
    frame.iter().find_map(|command| match command {
        DirectFrameCommand::BeginFrame {
            width,
            height,
            clear,
        } => Some((*width, *height, *clear)),
        _ => None,
    })
}

fn command_texture(command: &DirectFrameCommand) -> Option<TextureId> {
    match command {
        DirectFrameCommand::DrawSprite(command) => Some(command.texture),
        _ => None,
    }
}

fn command_bounds(
    command: &DirectFrameCommand,
    width: u32,
    height: u32,
) -> Option<HostedDamageRect> {
    let (min_x, min_y, max_x, max_y, scissor) = match command {
        DirectFrameCommand::DrawSprite(command) => {
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for vertex in command.vertices {
                if !vertex.position[0].is_finite() || !vertex.position[1].is_finite() {
                    return Some(HostedDamageRect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    });
                }
                min_x = min_x.min(vertex.position[0]);
                min_y = min_y.min(vertex.position[1]);
                max_x = max_x.max(vertex.position[0]);
                max_y = max_y.max(vertex.position[1]);
            }
            (min_x, min_y, max_x, max_y, command.scissor)
        }
        DirectFrameCommand::DrawSolid(command) => (
            command.rect.x as f32,
            command.rect.y as f32,
            command.rect.x.saturating_add(command.rect.width) as f32,
            command.rect.y.saturating_add(command.rect.height) as f32,
            command.scissor,
        ),
        _ => return None,
    };
    let mut left = min_x.floor().max(0.0) as i64;
    let mut top = min_y.floor().max(0.0) as i64;
    let mut right = max_x.ceil().max(0.0) as i64;
    let mut bottom = max_y.ceil().max(0.0) as i64;
    if let Some(RectI32 {
        x,
        y,
        width: clip_width,
        height: clip_height,
    }) = scissor
    {
        left = left.max(i64::from(x));
        top = top.max(i64::from(y));
        right = right.min(i64::from(x).saturating_add(i64::from(clip_width)));
        bottom = bottom.min(i64::from(y).saturating_add(i64::from(clip_height)));
    }
    left = left.clamp(0, i64::from(width));
    top = top.clamp(0, i64::from(height));
    right = right.clamp(0, i64::from(width));
    bottom = bottom.clamp(0, i64::from(height));
    if right <= left || bottom <= top {
        return None;
    }
    Some(HostedDamageRect {
        x: left as u32,
        y: top as u32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

fn union_rect(current: Option<HostedDamageRect>, next: HostedDamageRect) -> HostedDamageRect {
    let Some(current) = current else {
        return next;
    };
    let left = current.x.min(next.x);
    let top = current.y.min(next.y);
    let right = current
        .x
        .saturating_add(current.width)
        .max(next.x.saturating_add(next.width));
    let bottom = current
        .y
        .saturating_add(current.height)
        .max(next.y.saturating_add(next.height));
    HostedDamageRect {
        x: left,
        y: top,
        width: right.saturating_sub(left),
        height: bottom.saturating_sub(top),
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
    audio_bytes: usize,
    operations: Vec<HostedAudioOperation>,
    failure: Option<RfvpError>,
}

impl RecordingAudio {
    fn new() -> Self {
        Self {
            audio_bytes: 0,
            operations: Vec::new(),
            failure: None,
        }
    }

    fn push(&mut self, operation: HostedAudioOperation) -> RfvpResult<()> {
        self.finish()?;
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
        Ok(())
    }
}

impl RfvpAudio for RecordingAudio {
    fn load_resource(
        &mut self,
        id: AudioStreamId,
        kind: EncodedAudioKind,
        resource_uri: &str,
    ) -> RfvpResult<()> {
        if resource_uri.is_empty() {
            return Err(RfvpError::InvalidArgument);
        }
        self.push(HostedAudioOperation::LoadResource {
            id,
            kind,
            resource_uri: resource_uri.into(),
        })
    }

    fn load_encoded(
        &mut self,
        id: AudioStreamId,
        kind: EncodedAudioKind,
        bytes: &[u8],
    ) -> RfvpResult<()> {
        self.reserve_audio_bytes(bytes.len())?;
        self.push(HostedAudioOperation::LoadEncoded {
            id,
            kind,
            bytes: bytes.to_vec(),
        })
    }

    fn load_encoded_owned(
        &mut self,
        id: AudioStreamId,
        kind: EncodedAudioKind,
        bytes: Vec<u8>,
    ) -> RfvpResult<()> {
        self.reserve_audio_bytes(bytes.len())?;
        self.push(HostedAudioOperation::LoadEncoded { id, kind, bytes })
    }

    fn create_stream(&mut self, id: AudioStreamId, desc: AudioStreamDesc) -> RfvpResult<()> {
        self.push(HostedAudioOperation::CreateStream { id, desc })
    }

    fn submit_i16(&mut self, id: AudioStreamId, samples: &[i16]) -> RfvpResult<()> {
        let bytes = samples
            .len()
            .checked_mul(core::mem::size_of::<i16>())
            .ok_or(RfvpError::CapacityExceeded)?;
        self.reserve_audio_bytes(bytes)?;
        self.push(HostedAudioOperation::SubmitI16 {
            id,
            samples: samples.to_vec(),
        })
    }

    fn submit_i16_owned(&mut self, id: AudioStreamId, samples: Vec<i16>) -> RfvpResult<()> {
        let bytes = samples
            .len()
            .checked_mul(core::mem::size_of::<i16>())
            .ok_or(RfvpError::CapacityExceeded)?;
        self.reserve_audio_bytes(bytes)?;
        self.push(HostedAudioOperation::SubmitI16 { id, samples })
    }

    fn submit_f32(&mut self, id: AudioStreamId, samples: &[f32]) -> RfvpResult<()> {
        let bytes = samples
            .len()
            .checked_mul(core::mem::size_of::<f32>())
            .ok_or(RfvpError::CapacityExceeded)?;
        self.reserve_audio_bytes(bytes)?;
        self.push(HostedAudioOperation::SubmitF32 {
            id,
            samples: samples.to_vec(),
        })
    }

    fn submit_f32_owned(&mut self, id: AudioStreamId, samples: Vec<f32>) -> RfvpResult<()> {
        let bytes = samples
            .len()
            .checked_mul(core::mem::size_of::<f32>())
            .ok_or(RfvpError::CapacityExceeded)?;
        self.reserve_audio_bytes(bytes)?;
        self.push(HostedAudioOperation::SubmitF32 { id, samples })
    }

    fn play(&mut self, id: AudioStreamId, params: AudioParams, fade_in_ms: u32) -> RfvpResult<()> {
        self.push(HostedAudioOperation::Play {
            id,
            params,
            fade_in_ms,
        })
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
