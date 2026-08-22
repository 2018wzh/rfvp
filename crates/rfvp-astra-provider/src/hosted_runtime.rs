//! Thread-confined RFVP hosted-session lifecycle.
//!
//! This is the concrete v7 execution boundary used by the dynamic provider:
//! the RFVP core and its non-`Send` VFS cursors remain on one worker while the
//! caller exchanges only bounded semantic deltas and opaque snapshots.

use std::collections::BTreeMap;

use astra_emu_family_api::LegacyVfsReader;
use rfvp_hosted::{
    hosted::{
        HostedBootConfig, HostedConfig, HostedLimits, HostedSession, HostedStepDelta,
        HostedStepInput, HostedTraceProfile,
    },
    script::parser::Nls,
    soft_render::PixelFormat,
};
use thiserror::Error;

use crate::{
    hosted_host::HostedMemoryHost,
    hosted_worker::{HostedSessionWorker, HostedWorkerError, HostedWorkerStartError},
    FvpNls,
};

pub const MAX_HOSTED_CASE_FILES: usize = 65_536;
pub const MAX_HOSTED_HCB_BYTES: usize = 512 * 1024 * 1024;

pub struct HostedVfsSessionConfig {
    pub reader: std::sync::Arc<dyn LegacyVfsReader>,
    pub mount_set_id: String,
    pub expected_script_uri: String,
    pub pack_paths: Vec<String>,
    pub nls: FvpNls,
    pub stage_width: u32,
    pub stage_height: u32,
    pub trace_profile: HostedTraceProfile,
}

#[derive(Debug, Error)]
pub enum HostedRuntimeError {
    #[error("hosted case script URI is invalid")]
    ScriptUri,
    #[error("hosted case script conflicts with a supplied file")]
    ScriptCollision,
    #[error("hosted core booted a different HCB than the session binding")]
    ScriptBinding,
    #[error("hosted session initialization failed: {0}")]
    Initialization(String),
    #[error(transparent)]
    Worker(#[from] HostedWorkerError),
    #[error("hosted core failed: {0:?}")]
    Core(rfvp_hosted::host_api::RfvpError),
}

struct HostedState {
    core: HostedSession,
    host: HostedMemoryHost,
}

/// Sendable owner of one non-Send RFVP hosted-core session.
pub struct HostedFvpSession {
    worker: HostedSessionWorker<HostedState>,
}

impl HostedFvpSession {
    pub fn open_case(
        mut files: BTreeMap<String, Vec<u8>>,
        script_uri: String,
        script_bytes: Vec<u8>,
        nls: FvpNls,
        stage_width: u32,
        stage_height: u32,
        trace_profile: HostedTraceProfile,
    ) -> Result<Self, HostedRuntimeError> {
        if !script_uri.ends_with(".hcb") {
            return Err(HostedRuntimeError::ScriptUri);
        }
        if let Some(existing) = files.insert(script_uri, script_bytes.clone()) {
            if existing != script_bytes {
                return Err(HostedRuntimeError::ScriptCollision);
            }
        }
        let worker = HostedSessionWorker::try_spawn(move || {
            let mut host = HostedMemoryHost::new(files).map_err(HostedRuntimeError::Core)?;
            let mut core = HostedSession::new(
                HostedConfig {
                    virtual_width: stage_width,
                    virtual_height: stage_height,
                    ..HostedConfig::default()
                },
                HostedLimits::default(),
            )
            .map_err(HostedRuntimeError::Core)?;
            core.set_trace_profile(trace_profile)
                .map_err(HostedRuntimeError::Core)?;
            if let Err(error) = core.boot(
                &mut host,
                HostedBootConfig {
                    asset_root: ".",
                    hcb_extension: "hcb",
                    max_hcb_bytes: MAX_HOSTED_HCB_BYTES,
                    max_manifest_entries: MAX_HOSTED_CASE_FILES,
                    nls: map_nls(nls),
                },
            ) {
                let detail = core.core().last_error_detail().unwrap_or("unspecified");
                return Err(HostedRuntimeError::Initialization(format!(
                    "ASTRA_FVP_HOSTED_BOOT:{error:?}:{detail}"
                )));
            }
            core.use_direct_surface();
            Ok::<_, HostedRuntimeError>(HostedState { core, host })
        })
        .map_err(map_start_error)?;
        Ok(Self { worker })
    }

    /// Opens a dynamic session through the bounded host VFS port.  The
    /// requested HCB is checked after boot because RFVP discovers scripts by
    /// extension; accepting a different discovered script would break the
    /// package binding even if both files are otherwise valid.
    pub fn open_vfs(config: HostedVfsSessionConfig) -> Result<Self, HostedRuntimeError> {
        let expected_script_uri = normalize_script_uri(&config.expected_script_uri)?;
        let worker = HostedSessionWorker::try_spawn(move || {
            let mut host =
                HostedMemoryHost::from_vfs(config.reader, config.mount_set_id, config.pack_paths)
                    .map_err(HostedRuntimeError::Core)?;
            let mut core = HostedSession::new(
                HostedConfig {
                    virtual_width: config.stage_width,
                    virtual_height: config.stage_height,
                    ..HostedConfig::default()
                },
                HostedLimits::default(),
            )
            .map_err(HostedRuntimeError::Core)?;
            core.set_trace_profile(config.trace_profile)
                .map_err(HostedRuntimeError::Core)?;
            if let Err(error) = core.boot(
                &mut host,
                HostedBootConfig {
                    asset_root: ".",
                    hcb_extension: "hcb",
                    max_hcb_bytes: MAX_HOSTED_HCB_BYTES,
                    max_manifest_entries: MAX_HOSTED_CASE_FILES,
                    nls: map_nls(config.nls),
                },
            ) {
                let detail = core.core().last_error_detail().unwrap_or("unspecified");
                return Err(HostedRuntimeError::Initialization(format!(
                    "ASTRA_FVP_HOSTED_BOOT:{error:?}:{detail}"
                )));
            }
            if core
                .core()
                .loaded_game()
                .is_none_or(|loaded| loaded.hcb_path != expected_script_uri)
            {
                return Err(HostedRuntimeError::ScriptBinding);
            }
            core.use_direct_surface();
            Ok::<_, HostedRuntimeError>(HostedState { core, host })
        })
        .map_err(map_start_error)?;
        Ok(Self { worker })
    }

    pub fn step(
        &self,
        delta_ns: u64,
        input: HostedStepInput,
    ) -> Result<HostedStepDelta, HostedRuntimeError> {
        self.worker.execute_result(move |state| {
            state
                .host
                .advance(delta_ns)
                .map_err(HostedRuntimeError::Core)?;
            state
                .core
                .step(&mut state.host, input)
                .map_err(HostedRuntimeError::Core)
        })?
    }

    pub fn replace_text(&self, slot: u8, text: String) -> Result<(), HostedRuntimeError> {
        self.worker.execute_result(move |state| {
            state
                .core
                .replace_text(slot, &text)
                .map_err(HostedRuntimeError::Core)
        })?
    }

    pub fn render_surface(
        &self,
        width: u32,
        height: u32,
        format: PixelFormat,
        pixels: astra_byte_source::OwnedWritableByteBuffer,
    ) -> Result<astra_byte_source::OwnedWritableByteBuffer, HostedRuntimeError> {
        self.worker.execute_result(move |state| {
            state
                .core
                .render_direct_surface(width, height, format, pixels)
                .map_err(HostedRuntimeError::Core)
        })?
    }

    pub fn complete_video(&self) -> Result<(), HostedRuntimeError> {
        self.worker.execute_result(|state| {
            state
                .core
                .complete_video()
                .map_err(HostedRuntimeError::Core)
        })?
    }

    pub fn quit_requested(&self) -> Result<bool, HostedRuntimeError> {
        self.worker
            .execute_result(|state| Ok::<_, HostedRuntimeError>(state.core.quit_requested()))?
    }

    pub fn is_terminal(&self) -> Result<bool, HostedRuntimeError> {
        self.worker
            .execute_result(|state| Ok::<_, HostedRuntimeError>(state.core.is_terminal()))?
    }

    pub fn evidence_vm_trace(
        &self,
    ) -> Result<Vec<rfvp_hosted::hosted::HostedVmTraceRecord>, HostedRuntimeError> {
        self.worker
            .execute_result(|state| state.core.crash_trace().map_err(HostedRuntimeError::Core))?
    }

    pub fn shutdown(self) -> Result<(), HostedRuntimeError> {
        self.worker.shutdown().map_err(HostedRuntimeError::Worker)
    }
}

fn map_start_error(error: HostedWorkerStartError<HostedRuntimeError>) -> HostedRuntimeError {
    match error {
        HostedWorkerStartError::Initialization(error) => error,
        HostedWorkerStartError::Worker(error) => HostedRuntimeError::Worker(error),
    }
}

fn map_nls(nls: FvpNls) -> Nls {
    match nls {
        FvpNls::ShiftJis => Nls::ShiftJIS,
        FvpNls::Gbk => Nls::GBK,
        FvpNls::Utf8 => Nls::UTF8,
    }
}

fn normalize_script_uri(uri: &str) -> Result<String, HostedRuntimeError> {
    if !uri.ends_with(".hcb") || uri.is_empty() || uri.contains('\\') {
        return Err(HostedRuntimeError::ScriptUri);
    }
    let uri = uri.strip_prefix("./").unwrap_or(uri);
    if uri
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(HostedRuntimeError::ScriptUri);
    }
    Ok(uri.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_lifecycle_renders_directly_into_the_supplied_allocation() {
        let session = HostedFvpSession::open_case(
            BTreeMap::from([(
                "default.ttf".into(),
                include_bytes!(
                    "../../../../../Engine/Fixtures/PublicDomainFonts/NotoSansSC-Variable.ttf"
                )
                .to_vec(),
            )]),
            "script.hcb".into(),
            terminal_hcb(),
            FvpNls::Utf8,
            1024,
            768,
            HostedTraceProfile::Shipping,
        )
        .expect("hosted case must boot");
        let delta = session
            .step(16_666_667, HostedStepInput::default())
            .expect("hosted case must step");
        assert_eq!(delta.tick.frame_index, 1);
        let pixels = session
            .render_surface(
                1024,
                768,
                PixelFormat::Rgba8,
                vec![0; 1024 * 768 * 4].into(),
            )
            .expect("direct surface must render");
        assert_eq!(pixels.len(), 1024 * 768 * 4);
        assert!(!session
            .quit_requested()
            .expect("quit state must be readable"));
        session.shutdown().expect("worker must stop");
    }

    fn terminal_hcb() -> Vec<u8> {
        let mut bytes = 8u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0x04, 0, 0, 0]);
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&[8, 0, 2, b'X', 0]);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes
    }
}
