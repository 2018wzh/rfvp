#[cfg(feature = "no_std")]
use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use anyhow::{bail, Context, Result};
#[cfg(any(not(feature = "no_std"), feature = "hosted"))]
use bincode::Options;
use core::fmt;
use serde::{Deserialize, Serialize};

use crate::audio_player::{BgmPlayerSnapshotV1, SePlayerSnapshotV1};
#[cfg(not(feature = "hosted"))]
use crate::script::global::GLOBAL;
use crate::script::Variant;
use crate::subsystem::resources::motion_manager::MotionManagerSnapshotV2;
use crate::subsystem::resources::thread_manager::{ThreadManager, ThreadManagerSnapshotV1};
use crate::subsystem::world::GameData;

const SAVE_STATE_MAGIC: [u8; 4] = *b"RFVS";
const SAVE_STATE_FOOTER_LEN: usize = 8; // u32 payload_len + 4-byte magic
const MAX_STATE_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnsupportedSnapshotVersion(pub(crate) u16);

impl fmt::Display for UnsupportedSnapshotVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unsupported RFVS snapshot version: {}", self.0)
    }
}

impl core::error::Error for UnsupportedSnapshotVersion {}

#[cfg(any(not(feature = "no_std"), feature = "hosted"))]
fn bincode_opts() -> impl bincode::Options {
    bincode::DefaultOptions::new()
        .with_limit(MAX_STATE_PAYLOAD_BYTES as u64)
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioSnapshotV1 {
    pub bgm: BgmPlayerSnapshotV1,
    pub se: SePlayerSnapshotV1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveStateSnapshotV2 {
    pub version: u16,
    pub motion: MotionManagerSnapshotV2,
    pub audio: AudioSnapshotV1,
    /// Reverse-engineered slot save persists non-volatile globals together with the scene snapshot.
    pub globals_non_volatile: Vec<Variant>,
    /// Full VM coroutine state needed for resumable saves.
    pub vm: ThreadManagerSnapshotV1,
}

impl SaveStateSnapshotV2 {
    pub fn capture(game_data: &mut GameData) -> Self {
        let vm = match game_data.save_manager.take_pending_vm_snapshot() {
            Some(s) => s,
            None => {
                log::warn!("SaveStateSnapshotV2: missing VM snapshot; save will not be resumable");
                ThreadManager::new().capture_snapshot_v1()
            }
        };

        Self::capture_with_vm(game_data, vm)
    }

    /// Captures a coherent hosted checkpoint from the live VM.
    #[cfg(feature = "hosted")]
    pub fn capture_hosted(game_data: &GameData, vm: &ThreadManager) -> Self {
        Self::capture_with_vm(game_data, vm.capture_snapshot_v1())
    }

    #[cfg(feature = "hosted")]
    pub fn capture_user_save(game_data: &mut GameData) -> Result<Self> {
        let vm = game_data
            .save_manager
            .take_pending_vm_snapshot()
            .context("user save has no prepared VM snapshot")?;
        let snapshot = Self::capture_with_vm(game_data, vm);
        snapshot.trace_text_state("capture");
        Ok(snapshot)
    }

    /// Only numeric style and reveal state cross the diagnostic boundary.
    #[cfg(feature = "hosted")]
    pub(crate) fn trace_text_state(&self, operation: &'static str) {
        if !tracing::enabled!(target: "rfvp::save", tracing::Level::DEBUG) {
            return;
        }
        for (slot, text) in self.motion.text_manager.items.iter().enumerate() {
            if !text.loaded {
                continue;
            }
            let rgba = |color: &crate::subsystem::resources::color_manager::ColorItem| {
                u32::from_be_bytes([color.get_r(), color.get_g(), color.get_b(), color.get_a()])
            };
            tracing::debug!(
                target: "rfvp::save",
                event = "rfvp.save.text_state",
                operation,
                slot,
                font = text.text_font_idx1,
                size = text.text_size1,
                outline = text.outline_size1,
                fill_rgba = rgba(&text.color1),
                outline_rgba = rgba(&text.color2),
                shadow_rgba = rgba(&text.color3),
                suspended = text.is_suspended,
                visible_columns = text.visible_chars,
                total_columns = text.total_chars,
            );
        }
    }

    fn capture_with_vm(game_data: &GameData, vm: ThreadManagerSnapshotV1) -> Self {
        let globals_non_volatile = {
            #[cfg(feature = "hosted")]
            let g = &game_data.globals;
            #[cfg(not(feature = "hosted"))]
            let g = GLOBAL.lock().unwrap();
            g.snapshot_non_volatile()
        };
        Self {
            version: 2,
            motion: game_data.motion_manager.capture_snapshot_v2(),
            audio: AudioSnapshotV1 {
                bgm: game_data.bgm_player_ref().capture_snapshot_v1(),
                se: game_data.se_player_ref().capture_snapshot_v1(),
            },
            globals_non_volatile,
            vm,
        }
    }

    pub fn apply(&self, game_data: &mut GameData, tm: &mut ThreadManager) -> Result<()> {
        let GameData {
            motion_manager,
            vfs,
            bgm_player,
            se_player,
            ..
        } = game_data;

        motion_manager
            .apply_snapshot_v2(&self.motion, vfs)
            .context("apply MotionManagerSnapshotV2")?;

        bgm_player
            .apply_snapshot_v1(&self.audio.bgm, vfs)
            .context("apply BgmPlayerSnapshotV1")?;

        se_player
            .apply_snapshot_v1(&self.audio.se, vfs)
            .context("apply SePlayerSnapshotV1")?;

        {
            #[cfg(feature = "hosted")]
            let g = &mut game_data.globals;
            #[cfg(not(feature = "hosted"))]
            let mut g = GLOBAL.lock().unwrap();
            #[cfg(feature = "hosted")]
            if self.globals_non_volatile.len() != usize::from(g.non_volatile_count()) {
                bail!("hosted snapshot global count is incompatible with this session");
            }
            g.restore_non_volatile(&self.globals_non_volatile);
        }

        tm.apply_snapshot_v1(&self.vm);
        Ok(())
    }
}

pub fn append_state_chunk_v2(out: &mut Vec<u8>, snap: &SaveStateSnapshotV2) -> Result<()> {
    #[cfg(all(feature = "no_std", not(feature = "hosted")))]
    {
        let _ = (out, snap);
        bail!("SaveStateSnapshotV2 serialization requires the std desktop bincode adapter");
    }

    #[cfg(any(not(feature = "no_std"), feature = "hosted"))]
    {
        let payload = bincode_opts()
            .serialize(snap)
            .context("serialize SaveStateSnapshotV2")?;

        if payload.len() > MAX_STATE_PAYLOAD_BYTES {
            bail!(
                "SaveStateSnapshotV2 payload too large: {} bytes (max {})",
                payload.len(),
                MAX_STATE_PAYLOAD_BYTES
            );
        }

        let len_u32: u32 = payload
            .len()
            .try_into()
            .context("payload length overflow")?;
        out.extend_from_slice(&payload);
        out.extend_from_slice(&len_u32.to_le_bytes());
        out.extend_from_slice(&SAVE_STATE_MAGIC);
        Ok(())
    }
}

pub fn try_decode_state_chunk_v2(file_bytes: &[u8]) -> Result<Option<SaveStateSnapshotV2>> {
    #[cfg(all(feature = "no_std", not(feature = "hosted")))]
    {
        let _ = file_bytes;
        return Ok(None);
    }

    #[cfg(any(not(feature = "no_std"), feature = "hosted"))]
    {
        if file_bytes.len() < SAVE_STATE_FOOTER_LEN {
            return Ok(None);
        }

        let magic_pos = file_bytes.len() - 4;
        if file_bytes[magic_pos..] != SAVE_STATE_MAGIC {
            return Ok(None);
        }

        let len_pos = file_bytes.len() - SAVE_STATE_FOOTER_LEN;
        let payload_len = u32::from_le_bytes([
            file_bytes[len_pos],
            file_bytes[len_pos + 1],
            file_bytes[len_pos + 2],
            file_bytes[len_pos + 3],
        ]) as usize;

        if payload_len > MAX_STATE_PAYLOAD_BYTES {
            bail!(
                "SaveState footer present but payload length {} exceeds max {}",
                payload_len,
                MAX_STATE_PAYLOAD_BYTES
            );
        }

        if file_bytes.len() < SAVE_STATE_FOOTER_LEN + payload_len {
            bail!(
            "SaveState footer present but payload is truncated: file={} need={} payload_len={} footer={}",
            file_bytes.len(),
            SAVE_STATE_FOOTER_LEN + payload_len,
            payload_len,
            SAVE_STATE_FOOTER_LEN
        );
        }

        let payload_start = file_bytes.len() - SAVE_STATE_FOOTER_LEN - payload_len;
        let payload_end = payload_start + payload_len;
        let payload = &file_bytes[payload_start..payload_end];

        if let Some(version_bytes) = payload.get(..2) {
            let version = u16::from_le_bytes([version_bytes[0], version_bytes[1]]);
            if version != 2 {
                return Err(anyhow::Error::new(UnsupportedSnapshotVersion(version)));
            }
        }

        let snap: SaveStateSnapshotV2 = bincode_opts()
            .deserialize(payload)
            .context("deserialize SaveStateSnapshotV2")?;
        if snap.version != 2 {
            return Err(anyhow::Error::new(UnsupportedSnapshotVersion(snap.version)));
        }
        Ok(Some(snap))
    }
}

#[cfg(all(test, any(not(feature = "no_std"), feature = "hosted")))]
mod tests {
    use super::{
        append_state_chunk_v2, try_decode_state_chunk_v2, SaveStateSnapshotV2,
        UnsupportedSnapshotVersion, SAVE_STATE_FOOTER_LEN, SAVE_STATE_MAGIC,
    };
    use crate::subsystem::resources::thread_manager::ThreadManager;
    use crate::subsystem::world::GameData;

    fn valid_v2_chunk() -> Vec<u8> {
        let snapshot = {
            #[cfg(feature = "hosted")]
            {
                SaveStateSnapshotV2::capture_hosted(&GameData::default(), &ThreadManager::new())
            }
            #[cfg(not(feature = "hosted"))]
            {
                let mut game = GameData::default();
                SaveStateSnapshotV2::capture(&mut game)
            }
        };
        let mut bytes = Vec::new();
        append_state_chunk_v2(&mut bytes, &snapshot).unwrap();
        bytes
    }

    fn chunk_with_payload(payload: &[u8]) -> Vec<u8> {
        let mut bytes = payload.to_vec();
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&SAVE_STATE_MAGIC);
        bytes
    }

    fn payload_start(bytes: &[u8]) -> usize {
        let len_pos = bytes.len() - SAVE_STATE_FOOTER_LEN;
        let payload_len = u32::from_le_bytes([
            bytes[len_pos],
            bytes[len_pos + 1],
            bytes[len_pos + 2],
            bytes[len_pos + 3],
        ]) as usize;
        bytes.len() - SAVE_STATE_FOOTER_LEN - payload_len
    }

    #[test]
    fn unsupported_version_is_typed_and_does_not_mutate_input() {
        let mut bytes = valid_v2_chunk();
        let start = payload_start(&bytes);
        bytes[start..start + 2].copy_from_slice(&1u16.to_le_bytes());
        let before = bytes.clone();

        let error = try_decode_state_chunk_v2(&bytes).unwrap_err();

        assert_eq!(bytes, before);
        assert_eq!(
            error.downcast_ref::<UnsupportedSnapshotVersion>(),
            Some(&UnsupportedSnapshotVersion(1))
        );
    }

    #[test]
    fn short_or_malformed_v2_payload_is_invalid_data() {
        for payload in [vec![1u8], vec![2u8, 0]] {
            let error = try_decode_state_chunk_v2(&chunk_with_payload(&payload)).unwrap_err();
            assert!(error.downcast_ref::<UnsupportedSnapshotVersion>().is_none());
        }
    }

    #[test]
    fn valid_v2_payload_decodes() {
        let snapshot = try_decode_state_chunk_v2(&valid_v2_chunk())
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.version, 2);
    }
}
