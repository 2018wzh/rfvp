//! RFVP-owned conversion of hosted audio and video output into Family ABI v9 DTOs.

use astra_emu_family_api::{
    LegacyAudioCommandV1, LegacyAudioEncoding, LegacyAudioSampleFormat, LegacyVideoCommandV1,
    LegacyVideoMode,
};
use rfvp_hosted::hosted::{HostedAudioOperation, HostedVideoOperation};

pub fn video_commands_from_delta(
    frame_index: u64,
    operations: Vec<HostedVideoOperation>,
) -> Result<Vec<LegacyVideoCommandV1>, HostedAdapterError> {
    operations
        .into_iter()
        .enumerate()
        .map(|(index, operation)| match operation {
            HostedVideoOperation::Play {
                resource_uri,
                byte_len,
                modal_with_audio,
                stage_width,
                stage_height,
            } => {
                if byte_len == 0 || byte_len > 512 * 1024 * 1024 {
                    return Err(HostedAdapterError::VideoResourceBounds);
                }
                let command = LegacyVideoCommandV1::Play {
                    playback_id: format!("rfvp-{frame_index}-{index}"),
                    resource_uri,
                    mode: if modal_with_audio {
                        LegacyVideoMode::ModalWithAudio
                    } else {
                        LegacyVideoMode::LayerNoAudio
                    },
                    stage_width,
                    stage_height,
                };
                command
                    .validate()
                    .map_err(|error| HostedAdapterError::InvalidPacket(error.code().to_owned()))?;
                Ok(command)
            }
        })
        .collect()
}

/// Converts the hosted core's single audio transaction into validated host
/// commands.  This keeps PCM/encoded buffers bounded by RFVP and avoids the
/// former second audio DTO and cross-layer command mutex.
pub fn audio_commands_from_delta(
    operations: Vec<HostedAudioOperation>,
) -> Result<Vec<LegacyAudioCommandV1>, HostedAdapterError> {
    operations
        .into_iter()
        .map(|operation| {
            let command = match operation {
                HostedAudioOperation::LoadResource {
                    id,
                    kind,
                    resource_uri,
                } => LegacyAudioCommandV1::LoadResource {
                    stream_id: id.0,
                    encoding: match kind {
                        rfvp_hosted::host_api::EncodedAudioKind::Unknown => {
                            LegacyAudioEncoding::Unknown
                        }
                        rfvp_hosted::host_api::EncodedAudioKind::Wav => LegacyAudioEncoding::Wav,
                        rfvp_hosted::host_api::EncodedAudioKind::Ogg => LegacyAudioEncoding::Ogg,
                        rfvp_hosted::host_api::EncodedAudioKind::Mp3 => LegacyAudioEncoding::Mp3,
                        rfvp_hosted::host_api::EncodedAudioKind::Flac => LegacyAudioEncoding::Flac,
                    },
                    resource_uri,
                },
                HostedAudioOperation::LoadEncoded { .. } => {
                    return Err(HostedAdapterError::EncodedAudioRequiresResource);
                }
                HostedAudioOperation::CreateStream { id, desc } => {
                    LegacyAudioCommandV1::CreateStream {
                        stream_id: id.0,
                        sample_rate: desc.sample_rate,
                        channels: desc.channels,
                        sample_format: match desc.sample_format {
                            rfvp_hosted::host_api::AudioSampleFormat::I16 => {
                                LegacyAudioSampleFormat::I16
                            }
                            rfvp_hosted::host_api::AudioSampleFormat::F32 => {
                                LegacyAudioSampleFormat::F32
                            }
                        },
                    }
                }
                HostedAudioOperation::SubmitI16 { id, samples } => {
                    LegacyAudioCommandV1::SubmitI16 {
                        stream_id: id.0,
                        samples: samples.into(),
                    }
                }
                HostedAudioOperation::SubmitF32 { id, samples } => {
                    LegacyAudioCommandV1::SubmitF32 {
                        stream_id: id.0,
                        samples: samples.into(),
                    }
                }
                HostedAudioOperation::Play {
                    id,
                    params,
                    fade_in_ms,
                } => LegacyAudioCommandV1::Play {
                    stream_id: id.0,
                    volume: params.volume,
                    pan: params.pan,
                    repeat: params.repeat,
                    fade_in_ms,
                },
                HostedAudioOperation::Stop { id, fade_ms } => LegacyAudioCommandV1::Stop {
                    stream_id: id.0,
                    fade_ms,
                },
                HostedAudioOperation::Pause(id) => LegacyAudioCommandV1::Pause { stream_id: id.0 },
                HostedAudioOperation::Resume(id) => {
                    LegacyAudioCommandV1::Resume { stream_id: id.0 }
                }
                HostedAudioOperation::SetParams { id, params } => LegacyAudioCommandV1::SetParams {
                    stream_id: id.0,
                    volume: params.volume,
                    pan: params.pan,
                    repeat: params.repeat,
                },
                HostedAudioOperation::SetMasterVolume(volume) => {
                    LegacyAudioCommandV1::MasterVolume { volume }
                }
                HostedAudioOperation::DestroyStream(id) => {
                    LegacyAudioCommandV1::DestroyStream { stream_id: id.0 }
                }
                HostedAudioOperation::Tick { .. } => return Ok(None),
            };
            command
                .validate()
                .map_err(|error| HostedAdapterError::InvalidPacket(error.code().to_owned()))?;
            Ok(Some(command))
        })
        .filter_map(Result::transpose)
        .collect()
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HostedAdapterError {
    #[error("ASTRA_FVP_HOSTED_PACKET:{0}")]
    InvalidPacket(String),
    #[error("ASTRA_FVP_HOSTED_VIDEO_RESOURCE_BOUNDS")]
    VideoResourceBounds,
    #[error("ASTRA_FVP_HOSTED_AUDIO_RESOURCE_REQUIRED")]
    EncodedAudioRequiresResource,
}
