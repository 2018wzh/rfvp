use crate::host_api::*;
use crate::no_std_core::{RfvpBootConfig, RfvpCore, RfvpCoreConfig};
use crate::script::opcode::Opcode;
use alloc::{format, vec, vec::Vec};

struct MemoryFile(Vec<u8>);
impl RfvpFile for MemoryFile {
    fn len(&mut self) -> RfvpResult<u64> {
        Ok(self.0.len() as u64)
    }
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> RfvpResult<usize> {
        let start = (offset as usize).min(self.0.len());
        let len = out.len().min(self.0.len() - start);
        out[..len].copy_from_slice(&self.0[start..start + len]);
        Ok(len)
    }
}
struct Files(Vec<u8>);
impl RfvpFileSystem for Files {
    type File = MemoryFile;
    fn open(&mut self, path: &str) -> RfvpResult<MemoryFile> {
        match path {
            "input.hcb" => Ok(MemoryFile(self.0.clone())),
            "default.ttf" => Ok(MemoryFile(
                include_bytes!("../../rfvp-rebuilder/assets/fonts/BIZUDGothic-Regular.ttf")
                    .to_vec(),
            )),
            _ => Err(RfvpError::NotFound),
        }
    }
    fn metadata(&mut self, path: &str) -> RfvpResult<RfvpFileInfo> {
        self.open(path)
            .map(|file| RfvpFileInfo::file(file.0.len() as u64))
    }
    fn enumerate_by_extension(
        &mut self,
        _: &str,
        ext: &str,
        visit: &mut dyn FnMut(&str, RfvpFileInfo) -> RfvpResult<()>,
    ) -> RfvpResult<()> {
        if ext == "hcb" {
            visit("input.hcb", RfvpFileInfo::file(self.0.len() as u64))?;
        }
        Ok(())
    }
}
struct Audio;
impl RfvpAudio for Audio {
    fn load_encoded(&mut self, _: AudioStreamId, _: EncodedAudioKind, _: &[u8]) -> RfvpResult<()> {
        Err(RfvpError::Unsupported)
    }
    fn create_stream(&mut self, _: AudioStreamId, _: AudioStreamDesc) -> RfvpResult<()> {
        Err(RfvpError::Unsupported)
    }
    fn submit_i16(&mut self, _: AudioStreamId, _: &[i16]) -> RfvpResult<()> {
        Err(RfvpError::Unsupported)
    }
    fn submit_f32(&mut self, _: AudioStreamId, _: &[f32]) -> RfvpResult<()> {
        Err(RfvpError::Unsupported)
    }
    fn play(&mut self, _: AudioStreamId, _: AudioParams, _: u32) -> RfvpResult<()> {
        Err(RfvpError::Unsupported)
    }
    fn stop(&mut self, _: AudioStreamId, _: u32) -> RfvpResult<()> {
        Ok(())
    }
    fn pause(&mut self, _: AudioStreamId) -> RfvpResult<()> {
        Ok(())
    }
    fn resume(&mut self, _: AudioStreamId) -> RfvpResult<()> {
        Ok(())
    }
    fn set_params(&mut self, _: AudioStreamId, _: AudioParams) -> RfvpResult<()> {
        Ok(())
    }
    fn set_master_volume(&mut self, _: f32) -> RfvpResult<()> {
        Ok(())
    }
    fn destroy_stream(&mut self, _: AudioStreamId) {}
    fn tick(&mut self, _: u64) -> RfvpResult<()> {
        Ok(())
    }
}
struct Clock(u64);
impl RfvpClock for Clock {
    fn ticks_us(&mut self) -> u64 {
        self.0
    }
}
struct Host {
    fs: Files,
    renderer: SoftRaster,
    audio: Audio,
    clock: Clock,
}
impl RfvpHost for Host {
    type FileSystem = Files;
    type Renderer = SoftRaster;
    type Audio = Audio;
    type Clock = Clock;
    fn fs(&mut self) -> &mut Files {
        &mut self.fs
    }
    fn renderer(&mut self) -> &mut SoftRaster {
        &mut self.renderer
    }
    fn audio(&mut self) -> &mut Audio {
        &mut self.audio
    }
    fn clock(&mut self) -> &mut Clock {
        &mut self.clock
    }
}

#[test]
fn queued_input_is_visible_to_vm_once_per_tick() {
    let key_down = |repeat| RfvpEvent::KeyDown {
        key: KeyCode::Space,
        repeat,
        modifiers: InputModifiers::empty(),
    };
    let click = || {
        vec![
            RfvpEvent::PointerDown {
                button: PointerButton::Left,
                x: 2,
                y: 3,
            },
            RfvpEvent::PointerUp {
                button: PointerButton::Left,
                x: 2,
                y: 3,
            },
        ]
    };
    // state/down/up/repeat/wheel, observed by real VM syscalls before ThreadNext.
    let stages = vec![
        (vec![key_down(false)], [256, 256, 0, 256, 0]),
        (vec![], [256, 0, 0, 0, 0]),
        (vec![key_down(true)], [256, 0, 0, 256, 0]),
        (
            vec![RfvpEvent::KeyUp {
                key: KeyCode::Space,
                modifiers: InputModifiers::empty(),
            }],
            [0, 0, 256, 0, 0],
        ),
        (vec![], [0, 0, 0, 0, 0]),
        (
            vec![RfvpEvent::Wheel {
                delta_x: 0,
                delta_y: 120,
            }],
            [0, 0, 0, 0, 120],
        ),
        (vec![], [0, 0, 0, 0, 0]),
        (vec![key_down(false)], [256, 256, 0, 256, 0]),
        (vec![RfvpEvent::FocusLost], [0, 0, 0, 0, 0]),
        (vec![RfvpEvent::FocusGained], [0, 0, 0, 0, 0]),
        (click(), [0, 0, 0, 0, 0]),
        (click(), [0, 20, 20, 0, 0]),
        (
            vec![RfvpEvent::KeyDown {
                key: KeyCode::Character('x'),
                repeat: false,
                modifiers: InputModifiers::empty(),
            }],
            [0, 0, 0, 0, 0],
        ),
    ];
    let mut hcb = vec![0; 4];
    let mut failure_jumps = Vec::new();
    let mut checkpoints = Vec::new();
    for (_, expected) in &stages {
        for (id, value) in expected.iter().enumerate() {
            hcb.push(Opcode::Syscall as u8);
            hcb.extend_from_slice(&(id as u16).to_le_bytes());
            hcb.extend_from_slice(&[Opcode::PushReturn as u8, Opcode::PushI32 as u8]);
            hcb.extend_from_slice(&i32::to_le_bytes(*value));
            hcb.extend_from_slice(&[Opcode::SetE as u8, Opcode::Jz as u8]);
            failure_jumps.push(hcb.len());
            hcb.extend_from_slice(&0_u32.to_le_bytes());
        }
        hcb.extend_from_slice(&[Opcode::Syscall as u8, 5, 0]);
        checkpoints.push(hcb.len());
    }
    let end = hcb.len() as u32;
    hcb.extend_from_slice(&[Opcode::Syscall as u8, 5, 0, Opcode::Jmp as u8]);
    hcb.extend_from_slice(&end.to_le_bytes());
    let failure = hcb.len() as u32;
    // An assertion mismatch enters a distinct yielding PC, observable without test-only hooks.
    hcb.extend_from_slice(&[Opcode::Syscall as u8, 5, 0, Opcode::Jmp as u8]);
    hcb.extend_from_slice(&failure.to_le_bytes());
    for jump in failure_jumps {
        hcb[jump..jump + 4].copy_from_slice(&failure.to_le_bytes());
    }
    let descriptor = hcb.len() as u32;
    hcb[..4].copy_from_slice(&descriptor.to_le_bytes());
    hcb.extend_from_slice(&4_u32.to_le_bytes());
    hcb.extend_from_slice(&0_u16.to_le_bytes());
    hcb.extend_from_slice(&0_u16.to_le_bytes());
    hcb.extend_from_slice(&[0, 0, 1, 0]);
    let names = [
        "InputGetState",
        "InputGetDown",
        "InputGetUp",
        "InputGetRepeat",
        "InputGetWheel",
        "ThreadNext",
    ];
    hcb.extend_from_slice(&(names.len() as u16).to_le_bytes());
    for name in names {
        hcb.extend_from_slice(&[0, (name.len() + 1) as u8]);
        hcb.extend_from_slice(name.as_bytes());
        hcb.push(0);
    }
    hcb.extend_from_slice(&0_u16.to_le_bytes());
    let mut host = Host {
        fs: Files(hcb),
        renderer: SoftRaster::new(640, 480),
        audio: Audio,
        clock: Clock(0),
    };
    let mut core = RfvpCore::new(RfvpCoreConfig::default());
    #[cfg(feature = "hosted")]
    {
        use crate::subsystem::resources::text_manager::{SystemFontBindings, SystemFontFace};
        core.set_hosted_system_font_bindings(SystemFontBindings {
            ms_gothic: Some(SystemFontFace {
                family_name: "MS Gothic".into(),
                bytes: include_bytes!("subsystem/resources/fonts/MS-PGothic.ttf").to_vec(),
                face_index: 0,
            }),
            ..SystemFontBindings::default()
        })
        .unwrap();
    }
    core.boot(&mut host, RfvpBootConfig::default())
        .unwrap_or_else(|err| panic!("boot: {err:?}: {:?}", core.last_error_detail()));
    for (index, ((events, _), pc)) in stages.into_iter().zip(checkpoints).enumerate() {
        for event in events {
            core.push_event(event).unwrap();
        }
        host.clock.0 += 16_667;
        core.tick(&mut host).unwrap();
        assert!(
            core.debug_threads().contains(&format!("pc {pc:#x} ")),
            "VM input mismatch at tick {index}: {}",
            core.debug_threads()
        );
    }
}
