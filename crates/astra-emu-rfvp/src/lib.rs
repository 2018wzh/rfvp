//! Thin C ownership boundary around RFVP's native Embedded App and worker.
mod config;
mod input;
mod platform;

use astra_emu_abi::*;
use rfvp::app::embedded::session::Platform;
use rfvp::{
    app::{
        embedded::session::{Frame, NativeSession, RunState},
        App,
    },
    script::parser::{Nls, Parser},
};
use std::{
    ffi::c_void,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static PROBE_GAME_ID: parking_lot::Mutex<String> = parking_lot::Mutex::new(String::new());
struct Active;
impl Drop for Active {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Release);
    }
}
struct Handle {
    native: NativeSession,
    epoch: u64,
    poisoned: AtomicBool,
    _active: Active,
}

fn boundary(
    error: *mut Error,
    code: &'static str,
    work: impl FnOnce() -> anyhow::Result<Status>,
) -> Status {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok(Ok(status)) => status,
        Ok(Err(failure)) => {
            if let Some(error) = unsafe { error.as_mut() } {
                error.set(code, &format!("{failure:#}"));
            }
            CORE_ERROR
        }
        Err(_) => {
            if let Some(error) = unsafe { error.as_mut() } {
                error.set("RFVP_ABI_PANICKED", "native ABI operation panicked");
            }
            PANICKED
        }
    }
}
fn failure(error: *mut Error, status: Status, code: &str, message: &str) -> Status {
    if let Some(error) = unsafe { error.as_mut() } {
        error.set(code, message);
    }
    status
}
unsafe fn handle<'a>(session: Session) -> anyhow::Result<&'a Handle> {
    anyhow::ensure!(
        !session.is_null() && session.cast::<Handle>().is_aligned(),
        "RFVP_SESSION_INVALID"
    );
    Ok(unsafe { &*session.cast::<Handle>() })
}
fn surface(native: &NativeSession) -> SurfaceInfo {
    SurfaceInfo {
        kind: SURFACE_CPU,
        pixel_format: PIXEL_RGBA8,
        color: COLOR_SRGB,
        alpha: ALPHA_IGNORED,
        rows: ROWS_TOP_DOWN,
        logical: Extent {
            width: native.logical.0,
            height: native.logical.1,
        },
        raster: Extent {
            width: native.raster.0,
            height: native.raster.1,
        },
        stride: native.raster.0 as u64 * 4,
    }
}
fn initialized() -> anyhow::Result<()> {
    anyhow::ensure!(
        INITIALIZED.load(Ordering::Acquire),
        "ASTRA_EMU_FAMILY_DIAGNOSTIC_INIT"
    );
    Ok(())
}

unsafe extern "C" fn initialize(diagnostics: *const Diagnostics, error: *mut Error) -> Status {
    boundary(error, "ASTRA_EMU_FAMILY_DIAGNOSTIC_INIT", || {
        let diagnostics = unsafe { diagnostics.as_ref() }
            .ok_or_else(|| anyhow::anyhow!("missing diagnostic sink"))?;
        anyhow::ensure!(
            diagnostics.header.accepts::<Diagnostics>()
                && diagnostics.emit.is_some()
                && diagnostics.flush.is_some(),
            "invalid diagnostic sink"
        );
        #[cfg(feature = "dynamic-plugin-export")]
        unsafe { astra_emu_abi::diagnostic_bridge::install(*diagnostics) }.map_err(|status| {
            anyhow::anyhow!("diagnostic bridge initialization failed: {status}")
        })?;
        // Static linkage uses the host's subscriber, never installs a second logger.
        INITIALIZED.store(true, Ordering::Release);
        Ok(OK)
    })
}
unsafe extern "C" fn describe(output: *mut Descriptor, error: *mut Error) -> Status {
    boundary(error, "RFVP_DESCRIPTOR_FAILED", || {
        initialized()?;
        let output = unsafe { output.as_mut() }
            .ok_or_else(|| anyhow::anyhow!("missing descriptor output"))?;
        *output = config::descriptor();
        Ok(OK)
    })
}
unsafe extern "C" fn probe(path: Bytes, output: *mut ProbeResult, error: *mut Error) -> Status {
    boundary(error, "RFVP_PROBE_FAILED", || {
        initialized()?;
        let path = unsafe { path.text(32768) }.map_err(|_| anyhow::anyhow!("invalid game path"))?;
        let path = Path::new(path);
        let script = if path.is_dir() {
            match App::find_hcb(path) {
                Ok(path) => path,
                Err(_) => return Ok(NO_MATCH),
            }
        } else if path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("hcb") || s.eq_ignore_ascii_case("bch"))
        {
            path.to_path_buf()
        } else {
            return Ok(NO_MATCH);
        };
        Parser::new(&script, Nls::ShiftJIS)?;
        let mut game_id = PROBE_GAME_ID.lock();
        *game_id = script
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("RFVP_GAME_ID_INVALID"))?
            .to_owned();
        let output =
            unsafe { output.as_mut() }.ok_or_else(|| anyhow::anyhow!("missing probe output"))?;
        *output = ProbeResult {
            game_id: Bytes::new(game_id.as_bytes()),
            format: Bytes::new(b"fvp.hcb"),
            confidence_permyriad: 10000,
            reserved: 0,
        };
        Ok(OK)
    })
}
unsafe extern "C" fn open(
    request: *const OpenRequest,
    output: *mut OpenResult,
    error: *mut Error,
) -> Status {
    if output.is_null() {
        return failure(
            error,
            INVALID_ARGUMENT,
            "RFVP_OPEN_OUTPUT_INVALID",
            "missing open output",
        );
    }
    unsafe {
        (*output).session = std::ptr::null_mut();
    }
    boundary(error, "RFVP_OPEN_FAILED", || {
        initialized()?;
        let request =
            unsafe { request.as_ref() }.ok_or_else(|| anyhow::anyhow!("missing open request"))?;
        anyhow::ensure!(
            request.header.accepts::<OpenRequest>()
                && request.host.header.accepts::<HostServices>()
                && request.session_epoch != 0,
            "RFVP_OPEN_REQUEST_INVALID"
        );
        if !request.module_owner.is_null() {
            return Ok(failure(
                error,
                UNSUPPORTED,
                "RFVP_MODULE_OWNER_UNSUPPORTED",
                "this core has no module-owner extension",
            ));
        }
        anyhow::ensure!(
            request.host.cancellation.is_cancelled.is_some(),
            "RFVP_CANCELLATION_SERVICE_REQUIRED"
        );
        anyhow::ensure!(
            request.window.focused <= 1
                && request.window.suspended <= 1
                && request.window.scale_factor.is_finite()
                && request.window.scale_factor > 0.0,
            "RFVP_WINDOW_STATE_INVALID"
        );
        if config::RENDER
            .validate_request(config::CAPABILITIES, request.render_extent)
            .is_err()
        {
            return Ok(failure(
                error,
                INVALID_ARGUMENT,
                "RFVP_RENDER_EXTENT_INVALID",
                "render extent is outside the declared range",
            ));
        }
        let config = unsafe { config::parse(request)? };
        let path = unsafe { request.game_path.text(32768) }
            .map_err(|_| anyhow::anyhow!("RFVP_GAME_PATH_INVALID"))?;
        let path = Path::new(path);
        let root = if path.is_file() {
            path.parent()
                .ok_or_else(|| anyhow::anyhow!("RFVP_GAME_ROOT_INVALID"))?
        } else {
            path
        };
        anyhow::ensure!(root.is_dir(), "RFVP_GAME_ROOT_NOT_FOUND");
        let root = root
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("RFVP_GAME_ROOT_UTF8"))?
            .to_owned();
        anyhow::ensure!(
            ACTIVE
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "RFVP_SESSION_ALREADY_ACTIVE"
        );
        let active = Active;
        let platform = Arc::new(platform::HostWindow {
            service: request.host.window,
            cancellation: request.host.cancellation,
            session_epoch: request.session_epoch,
            next_request: AtomicU64::new(1),
        });
        let native = match NativeSession::open(
            root,
            config.nls,
            (request.render_extent.width, request.render_extent.height),
            config.translation_table,
            config.subtitle_map,
            platform.clone(),
            request.window.suspended != 0,
        ) {
            Ok(native) => native,
            Err(_) if platform.is_cancelled() => {
                return Ok(failure(
                    error,
                    CANCELLED,
                    "RFVP_SESSION_CANCELLED",
                    "native initialization cancelled",
                ))
            }
            Err(error) => return Err(error),
        };
        if platform.is_cancelled() {
            native.close()?;
            return Ok(failure(
                error,
                CANCELLED,
                "RFVP_SESSION_CANCELLED",
                "native initialization cancelled",
            ));
        }
        let info = surface(&native);
        info.validate()
            .map_err(|_| anyhow::anyhow!("RFVP_SURFACE_INVALID"))?;
        let context_epoch = native.snapshot().context_epoch;
        let owned = Box::new(Handle {
            native,
            epoch: request.session_epoch,
            poisoned: AtomicBool::new(false),
            _active: active,
        });
        unsafe {
            *output = OpenResult {
                session: Box::into_raw(owned).cast(),
                context_epoch,
                output_epoch: 1,
                surface: info,
            };
        }
        Ok(OK)
    })
}
unsafe extern "C" fn input(
    session: Session,
    events: *const InputEvent,
    count: usize,
    error: *mut Error,
) -> Status {
    let status = boundary(error, "RFVP_INPUT_FAILED", || {
        let handle = unsafe { handle(session)? };
        anyhow::ensure!(
            !handle.poisoned.load(Ordering::Acquire),
            "RFVP_SESSION_POISONED"
        );
        let events = match unsafe { borrowed_array(events, count, 256) } {
            Ok(events) => events,
            Err(status) => {
                return Ok(failure(
                    error,
                    status,
                    "RFVP_INPUT_INVALID",
                    "invalid input batch",
                ))
            }
        };
        let newest = handle
            .native
            .snapshot()
            .frame
            .as_ref()
            .map_or(0, |frame| frame.generation);
        let (native, suspended) = match unsafe { input::translate(events, handle.epoch, newest) } {
            Ok(inputs) => inputs,
            Err(status) => {
                return Ok(failure(
                    error,
                    status,
                    "RFVP_INPUT_INVALID",
                    "invalid or stale input event",
                ))
            }
        };
        if !native.is_empty() || suspended.is_some() {
            handle.native.input(native, suspended)?;
        }
        Ok(OK)
    });
    if status == PANICKED {
        if let Ok(handle) = unsafe { handle(session) } {
            handle.poisoned.store(true, Ordering::Release);
        }
    }
    status
}
unsafe extern "C" fn control(session: Session, operation: u32, error: *mut Error) -> Status {
    let status = boundary(error, "RFVP_CONTROL_FAILED", || {
        let handle = unsafe { handle(session)? };
        anyhow::ensure!(
            !handle.poisoned.load(Ordering::Acquire),
            "RFVP_SESSION_POISONED"
        );
        match operation {
            CONTROL_PAUSE => handle.native.set_paused(true)?,
            CONTROL_RESUME => handle.native.set_paused(false)?,
            _ => {
                return Ok(failure(
                    error,
                    UNSUPPORTED,
                    "RFVP_CONTROL_UNSUPPORTED",
                    "unsupported session control",
                ))
            }
        }
        Ok(OK)
    });
    if status == PANICKED {
        if let Ok(handle) = unsafe { handle(session) } {
            handle.poisoned.store(true, Ordering::Release);
        }
    }
    status
}
unsafe extern "C" fn state(
    session: Session,
    output: *mut SessionState,
    error: *mut Error,
) -> Status {
    boundary(error, "RFVP_STATE_FAILED", || {
        let handle = unsafe { handle(session)? };
        let output =
            unsafe { output.as_mut() }.ok_or_else(|| anyhow::anyhow!("missing state output"))?;
        let snapshot = handle.native.snapshot();
        let mut state = match snapshot.state {
            RunState::Running => SESSION_RUNNING,
            RunState::Paused => SESSION_PAUSED,
            RunState::Waiting => SESSION_WAITING,
            RunState::Failed => SESSION_FAILED,
            RunState::Finished => SESSION_FINISHED,
        };
        let mut native_error = Error::default();
        if let Some(message) = snapshot.error.as_ref() {
            native_error.set("RFVP_NATIVE_FAILED", message);
        }
        if handle.poisoned.load(Ordering::Acquire) {
            state = SESSION_FAILED;
            native_error.set("RFVP_ABI_PANICKED", "session is poisoned");
        }
        *output = SessionState {
            state,
            flags: 0,
            context_epoch: snapshot.context_epoch,
            output_epoch: 1,
            error: native_error,
        };
        Ok(OK)
    })
}
unsafe extern "C" fn release(owner: *mut c_void) {
    if !owner.is_null() {
        unsafe {
            drop(Arc::from_raw(owner.cast::<Frame>()));
        }
    }
}
unsafe extern "C" fn frame(
    session: Session,
    known: FrameIdentity,
    output: *mut FrameSurface,
    error: *mut Error,
) -> Status {
    boundary(error, "RFVP_FRAME_FAILED", || {
        let handle = unsafe { handle(session)? };
        let output =
            unsafe { output.as_mut() }.ok_or_else(|| anyhow::anyhow!("missing frame output"))?;
        let snapshot = handle.native.snapshot();
        let identity = FrameIdentity {
            session_epoch: handle.epoch,
            output_epoch: 1,
            generation: snapshot.frame.as_ref().map_or(0, |frame| frame.generation),
        };
        let mut result = FrameSurface {
            state: FRAME_PENDING,
            reserved: 0,
            identity,
            timestamp_ns: 0,
            info: surface(&handle.native),
            cpu: CpuLease {
                pixels: Bytes::EMPTY,
                owner: std::ptr::null_mut(),
                release: None,
            },
        };
        if let Some(frame) = snapshot.frame.as_ref() {
            result.timestamp_ns = frame.timestamp_ns;
            if known == identity {
                result.state = FRAME_UNCHANGED;
            } else {
                result.state = FRAME_READY;
                result.cpu = CpuLease {
                    pixels: Bytes::new(&frame.pixels),
                    owner: Arc::into_raw(frame.clone()).cast_mut().cast(),
                    release: Some(release),
                };
            }
        }
        *output = result;
        Ok(OK)
    })
}
unsafe extern "C" fn cancel(session: Session) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Ok(handle) = unsafe { handle(session) } {
            handle.native.cancel();
        }
    }));
}
unsafe extern "C" fn close(session: Session, error: *mut Error) -> Status {
    boundary(error, "RFVP_CLOSE_FAILED", || {
        anyhow::ensure!(
            !session.is_null() && session.cast::<Handle>().is_aligned(),
            "RFVP_SESSION_INVALID"
        );
        let handle = unsafe { Box::from_raw(session.cast::<Handle>()) };
        let result = handle.native.close();
        drop(handle);
        result?;
        Ok(OK)
    })
}
unsafe extern "C" fn extension(_name: Bytes, _version: u32, output: *mut *const c_void) -> Status {
    if output.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *output = std::ptr::null();
    }
    UNSUPPORTED
}

static API: CoreApi = CoreApi {
    header: Header::of::<CoreApi>(),
    initialize: Some(initialize),
    describe: Some(describe),
    probe: Some(probe),
    open: Some(open),
    input: Some(input),
    control: Some(control),
    state: Some(state),
    frame: Some(frame),
    cancel: Some(cancel),
    close: Some(close),
    extension: Some(extension),
};

/// Dynamic and static registration use the same table and lifecycle.
/// # Safety
/// `output` is writable storage for a single table pointer.
#[cfg_attr(feature = "dynamic-plugin-export", no_mangle)]
pub unsafe extern "C" fn astra_emu_query(version: u32, output: *mut *const CoreApi) -> Status {
    if output.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *output = std::ptr::null();
    }
    if version != ABI_VERSION {
        return UNSUPPORTED;
    }
    unsafe {
        *output = &API;
    }
    OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfvp::app::embedded::session::{Endpoint, PointerRequest};
    use std::sync::atomic::AtomicU32;
    use std::time::{Duration, Instant};
    use winit::{
        event::WindowEvent,
        keyboard::{Key, PhysicalKey},
    };

    struct NoHost;
    impl Platform for NoHost {
        fn window_mode(&self, _: i32, _: (u32, u32), _: u64) -> anyhow::Result<()> {
            panic!("a finished game must not request window changes")
        }
        fn pointer(&self, _: PointerRequest, _: u64) -> anyhow::Result<()> {
            panic!("a finished game must not request pointer changes")
        }
        fn invalidate(&self, _: u64) {}
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    /// Stands in for the native App: the game ends itself on its third frame, the
    /// frame on which it also asks for a window mode change.
    struct ExitingGame {
        steps: u32,
        shutdowns: Arc<AtomicU32>,
    }
    impl Endpoint for ExitingGame {
        fn logical_size(&self) -> (u32, u32) {
            (4, 4)
        }
        fn raster_size(&self) -> (u32, u32) {
            (4, 4)
        }
        fn context_epoch(&self) -> u64 {
            1
        }
        fn take_window_mode(&mut self) -> Option<i32> {
            (self.steps >= 3).then_some(2)
        }
        fn take_pointer(&mut self) -> Option<PointerRequest> {
            None
        }
        fn step(&mut self, _: Duration) -> anyhow::Result<bool> {
            self.steps += 1;
            Ok(self.steps >= 3)
        }
        fn set_paused(&mut self, _: bool) -> anyhow::Result<()> {
            Ok(())
        }
        fn input_key(&mut self, _: PhysicalKey, _: Key, _: bool, _: bool) {}
        fn input_window_event(&mut self, _: &WindowEvent) {}
        fn begin_capture(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn poll_capture(&mut self, _: &mut Vec<u8>) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn cancel_capture(&mut self) {}
        fn shutdown(&mut self) -> anyhow::Result<()> {
            self.shutdowns.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    #[test]
    fn game_exit_reports_finished_and_closes_cleanly() {
        let shutdowns = Arc::new(AtomicU32::new(0));
        let endpoint_shutdowns = shutdowns.clone();
        let native = NativeSession::open_endpoint(
            move |_| {
                Ok(ExitingGame {
                    steps: 0,
                    shutdowns: endpoint_shutdowns,
                })
            },
            Arc::new(NoHost),
            false,
        )
        .unwrap();
        assert!(ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok());
        let session: Session = Box::into_raw(Box::new(Handle {
            native,
            epoch: 7,
            poisoned: AtomicBool::new(false),
            _active: Active,
        }))
        .cast();

        let mut api = std::ptr::null();
        assert_eq!(unsafe { astra_emu_query(ABI_VERSION, &mut api) }, OK);
        let api = unsafe { &*api };
        let read_state = || {
            let mut state = SessionState {
                state: 0,
                flags: u32::MAX,
                context_epoch: 0,
                output_epoch: 0,
                error: Error::default(),
            };
            let mut error = Error::default();
            assert_eq!(
                unsafe { api.state.unwrap()(session, &mut state, &mut error) },
                OK
            );
            state
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let state = loop {
            let state = read_state();
            if state.state != SESSION_RUNNING || Instant::now() > deadline {
                break state;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!((state.state, state.flags), (SESSION_FINISHED, 0));

        // Host input and pause requests still in flight are accepted, not errors.
        let mut error = Error::default();
        let key = InputEvent {
            kind: INPUT_KEY,
            pressed: 1,
            repeat: 0,
            button: 0,
            physical_key: Bytes::new(b"Escape"),
            logical_key: Bytes::new(b"Escape"),
            x: 0.0,
            y: 0.0,
            displayed: FrameIdentity::default(),
            window: WindowState {
                drawable_width: 4,
                drawable_height: 4,
                scale_factor: 1.0,
                focused: 1,
                suspended: 0,
            },
        };
        assert_eq!(
            unsafe { api.input.unwrap()(session, &key, 1, &mut error) },
            OK
        );
        assert_eq!(
            unsafe { api.control.unwrap()(session, CONTROL_PAUSE, &mut error) },
            OK
        );
        assert_eq!(read_state().state, SESSION_FINISHED);

        assert_eq!(unsafe { api.close.unwrap()(session, &mut error) }, OK);
        assert_eq!(shutdowns.load(Ordering::Acquire), 1);
        assert!(!ACTIVE.load(Ordering::Acquire));
    }
}
