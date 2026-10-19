//! Thin C ownership boundary around RFVP's native Embedded App and worker.
mod config;
mod input;
mod platform;
#[cfg(windows)]
mod d3d12;

use astra_emu_abi::*;
use rfvp::app::embedded::session::Platform;
use rfvp::{app::{App, embedded::session::{Frame, NativeSession, RunState}}, script::parser::{Nls, Parser}};
use std::{ffi::c_void, path::Path, sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}}};

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static PROBE_GAME_ID: parking_lot::Mutex<String> = parking_lot::Mutex::new(String::new());
struct Active;
impl Drop for Active { fn drop(&mut self) { ACTIVE.store(false, Ordering::Release); } }
struct Handle {
    native: NativeSession,
    epoch: u64,
    poisoned: AtomicBool,
    _active: Active,
}

fn boundary(error: *mut Error, code: &'static str, work: impl FnOnce() -> anyhow::Result<Status>) -> Status {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok(Ok(status)) => status,
        Ok(Err(failure)) => {
            if let Some(error) = unsafe { error.as_mut() } { error.set(code, &format!("{failure:#}")); }
            CORE_ERROR
        }
        Err(_) => {
            if let Some(error) = unsafe { error.as_mut() } { error.set("RFVP_ABI_PANICKED", "native ABI operation panicked"); }
            PANICKED
        }
    }
}
fn failure(error: *mut Error, status: Status, code: &str, message: &str) -> Status {
    if let Some(error) = unsafe { error.as_mut() } { error.set(code, message); }
    status
}
unsafe fn handle<'a>(session: Session) -> anyhow::Result<&'a Handle> {
    anyhow::ensure!(!session.is_null() && session.cast::<Handle>().is_aligned(), "RFVP_SESSION_INVALID");
    Ok(unsafe { &*session.cast::<Handle>() })
}
fn surface(native: &NativeSession) -> SurfaceInfo {
    SurfaceInfo { kind: if native.gpu_delivery { SURFACE_GPU } else { SURFACE_CPU },
        pixel_format: PIXEL_RGBA8, color: COLOR_SRGB, alpha: ALPHA_IGNORED, rows: ROWS_TOP_DOWN,
        backend: if native.gpu_delivery { BACKEND_D3D12 } else { 0 },
        logical: Extent { width: native.logical.0, height: native.logical.1 },
        raster: Extent { width: native.raster.0, height: native.raster.1 },
        stride: if native.gpu_delivery { 0 } else { native.raster.0 as u64 * 4 } }
}
fn initialized() -> anyhow::Result<()> {
    anyhow::ensure!(INITIALIZED.load(Ordering::Acquire), "ASTRA_EMU_FAMILY_DIAGNOSTIC_INIT");
    Ok(())
}

unsafe extern "C" fn initialize(diagnostics: *const Diagnostics, error: *mut Error) -> Status {
    boundary(error, "ASTRA_EMU_FAMILY_DIAGNOSTIC_INIT", || {
        let diagnostics = unsafe { diagnostics.as_ref() }.ok_or_else(|| anyhow::anyhow!("missing diagnostic sink"))?;
        anyhow::ensure!(diagnostics.header.accepts::<Diagnostics>() && diagnostics.emit.is_some() && diagnostics.flush.is_some(), "invalid diagnostic sink");
        #[cfg(feature = "dynamic-plugin-export")]
        unsafe { astra_emu_abi::diagnostic_bridge::install(*diagnostics) }
            .map_err(|status| anyhow::anyhow!("diagnostic bridge initialization failed: {status}"))?;
        // Static linkage uses the host's subscriber, never installs a second logger.
        INITIALIZED.store(true, Ordering::Release);
        Ok(OK)
    })
}
unsafe extern "C" fn describe(output: *mut Descriptor, error: *mut Error) -> Status {
    boundary(error, "RFVP_DESCRIPTOR_FAILED", || {
        initialized()?;
        let output = unsafe { output.as_mut() }.ok_or_else(|| anyhow::anyhow!("missing descriptor output"))?;
        *output = config::descriptor();
        Ok(OK)
    })
}
unsafe extern "C" fn probe(path: Bytes, output: *mut ProbeResult, error: *mut Error) -> Status {
    boundary(error, "RFVP_PROBE_FAILED", || {
        initialized()?;
        let path = unsafe { path.text(32768) }.map_err(|_| anyhow::anyhow!("invalid game path"))?;
        let path = Path::new(path);
        let script = if path.is_dir() { match App::find_hcb(path) { Ok(path) => path, Err(_) => return Ok(NO_MATCH) } }
            else if path.extension().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case("hcb") || s.eq_ignore_ascii_case("bch")) { path.to_path_buf() }
            else { return Ok(NO_MATCH) };
        Parser::new(&script, Nls::ShiftJIS)?;
        let mut game_id = PROBE_GAME_ID.lock();
        *game_id = script.file_stem().and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("RFVP_GAME_ID_INVALID"))?.to_owned();
        let output = unsafe { output.as_mut() }.ok_or_else(|| anyhow::anyhow!("missing probe output"))?;
        *output = ProbeResult { game_id: Bytes::new(game_id.as_bytes()), format: Bytes::new(b"fvp.hcb"), confidence_permyriad: 10000, reserved: 0 };
        Ok(OK)
    })
}
unsafe extern "C" fn open(request: *const OpenRequest, output: *mut OpenResult, error: *mut Error) -> Status {
    unsafe { open_impl(request, std::ptr::null(), output, error) }
}
unsafe fn open_impl(request: *const OpenRequest, gpu_host: *const D3D12Host, output: *mut OpenResult, error: *mut Error) -> Status {
    if output.is_null() { return failure(error, INVALID_ARGUMENT, "RFVP_OPEN_OUTPUT_INVALID", "missing open output"); }
    unsafe { (*output).session = std::ptr::null_mut(); }
    boundary(error, "RFVP_OPEN_FAILED", || {
        initialized()?;
        let request = unsafe { request.as_ref() }.ok_or_else(|| anyhow::anyhow!("missing open request"))?;
        anyhow::ensure!(request.header.accepts::<OpenRequest>() && request.host.header.accepts::<HostServices>() && request.session_epoch != 0,
            "RFVP_OPEN_REQUEST_INVALID");
        if !request.module_owner.is_null() {
            return Ok(failure(error, UNSUPPORTED, "RFVP_MODULE_OWNER_UNSUPPORTED", "this core has no module-owner extension"));
        }
        anyhow::ensure!(request.host.cancellation.is_cancelled.is_some(), "RFVP_CANCELLATION_SERVICE_REQUIRED");
        anyhow::ensure!(request.window.focused <= 1 && request.window.suspended <= 1
            && request.window.scale_factor.is_finite() && request.window.scale_factor > 0.0, "RFVP_WINDOW_STATE_INVALID");
        let config = unsafe { config::parse(request)? };
        let delivery = match config.surface.as_str() {
            "cpu" => rfvp::app::embedded::Delivery::Cpu,
            #[cfg(windows)]
            "cpu_d3d12" => rfvp::app::embedded::Delivery::CpuD3D12,
            #[cfg(windows)]
            "d3d12" => {
                let Some(host) = (unsafe { gpu_host.as_ref() }) else {
                    return Ok(failure(error, UNSUPPORTED, "RFVP_D3D12_HOST_REQUIRED", "D3D12 delivery requires a compatible Host device"));
                };
                if host.validate().is_err() {
                    return Ok(failure(error, UNSUPPORTED, "RFVP_D3D12_HOST_VERSION", "incompatible D3D12 Host descriptor"));
                }
                rfvp::app::embedded::Delivery::D3D12(rfvp::app::embedded::d3d12::Host {
                    adapter_luid: host.adapter_luid, max_width: host.max_width, max_height: host.max_height,
                    dxgi_format: host.dxgi_format, copy_usage: host.copy_usage,
                })
            }
            _ => return Ok(failure(error, UNSUPPORTED, "RFVP_SURFACE_UNSUPPORTED", "unsupported frame delivery")),
        };
        let path = unsafe { request.game_path.text(32768) }.map_err(|_| anyhow::anyhow!("RFVP_GAME_PATH_INVALID"))?;
        let path = Path::new(path);
        let root = if path.is_file() { path.parent().ok_or_else(|| anyhow::anyhow!("RFVP_GAME_ROOT_INVALID"))? } else { path };
        anyhow::ensure!(root.is_dir(), "RFVP_GAME_ROOT_NOT_FOUND");
        let root = root.to_str().ok_or_else(|| anyhow::anyhow!("RFVP_GAME_ROOT_UTF8"))?.to_owned();
        anyhow::ensure!(ACTIVE.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok(), "RFVP_SESSION_ALREADY_ACTIVE");
        let active = Active;
        let platform = Arc::new(platform::HostWindow { service: request.host.window, cancellation: request.host.cancellation,
            session_epoch: request.session_epoch, next_request: AtomicU64::new(1) });
        let native = match NativeSession::open_with_delivery(root, config.nls, config.render_scale, config.translation_table, platform.clone(), request.window.suspended != 0, delivery) {
            Ok(native) => native,
            Err(_) if platform.is_cancelled() =>
                return Ok(failure(error, CANCELLED, "RFVP_SESSION_CANCELLED", "native initialization cancelled")),
            Err(error) => return Err(error),
        };
        if platform.is_cancelled() {
            native.close()?;
            return Ok(failure(error, CANCELLED, "RFVP_SESSION_CANCELLED", "native initialization cancelled"));
        }
        let info = surface(&native);
        info.validate().map_err(|_| anyhow::anyhow!("RFVP_SURFACE_INVALID"))?;
        let context_epoch = native.snapshot().context_epoch;
        let owned = Box::new(Handle { native, epoch: request.session_epoch, poisoned: AtomicBool::new(false), _active: active });
        unsafe { *output = OpenResult { session: Box::into_raw(owned).cast(), context_epoch, output_epoch: 1, surface: info }; }
        Ok(OK)
    })
}
unsafe extern "C" fn input(session: Session, events: *const InputEvent, count: usize, error: *mut Error) -> Status {
    let status = boundary(error, "RFVP_INPUT_FAILED", || {
        let handle = unsafe { handle(session)? };
        anyhow::ensure!(!handle.poisoned.load(Ordering::Acquire), "RFVP_SESSION_POISONED");
        let events = match unsafe { borrowed_array(events, count, 256) } {
            Ok(events) => events,
            Err(status) => return Ok(failure(error, status, "RFVP_INPUT_INVALID", "invalid input batch")),
        };
        let newest = handle.native.snapshot().frame.as_ref().map_or(0, |frame| frame.generation);
        let (native, suspended) = match unsafe { input::translate(events, handle.epoch, newest) } {
            Ok(inputs) => inputs,
            Err(status) => return Ok(failure(error, status, "RFVP_INPUT_INVALID", "invalid or stale input event")),
        };
        if !native.is_empty() || suspended.is_some() { handle.native.input(native, suspended)?; }
        Ok(OK)
    });
    if status == PANICKED { if let Ok(handle) = unsafe { handle(session) } { handle.poisoned.store(true, Ordering::Release); } }
    status
}
unsafe extern "C" fn control(session: Session, operation: u32, error: *mut Error) -> Status {
    let status = boundary(error, "RFVP_CONTROL_FAILED", || {
        let handle = unsafe { handle(session)? };
        anyhow::ensure!(!handle.poisoned.load(Ordering::Acquire), "RFVP_SESSION_POISONED");
        match operation {
            CONTROL_PAUSE => handle.native.set_paused(true)?,
            CONTROL_RESUME => handle.native.set_paused(false)?,
            _ => return Ok(failure(error, UNSUPPORTED, "RFVP_CONTROL_UNSUPPORTED", "unsupported session control")),
        }
        Ok(OK)
    });
    if status == PANICKED { if let Ok(handle) = unsafe { handle(session) } { handle.poisoned.store(true, Ordering::Release); } }
    status
}
unsafe extern "C" fn state(session: Session, output: *mut SessionState, error: *mut Error) -> Status {
    boundary(error, "RFVP_STATE_FAILED", || {
        let handle = unsafe { handle(session)? };
        let output = unsafe { output.as_mut() }.ok_or_else(|| anyhow::anyhow!("missing state output"))?;
        let snapshot = handle.native.snapshot();
        let mut state = match snapshot.state {
            RunState::Running => SESSION_RUNNING, RunState::Paused => SESSION_PAUSED, RunState::Waiting => SESSION_WAITING,
            RunState::Failed => SESSION_FAILED, RunState::Finished => SESSION_FINISHED,
        };
        let mut native_error = Error::default();
        if let Some(message) = snapshot.error.as_ref() { native_error.set("RFVP_NATIVE_FAILED", message); }
        if handle.poisoned.load(Ordering::Acquire) { state = SESSION_FAILED; native_error.set("RFVP_ABI_PANICKED", "session is poisoned"); }
        *output = SessionState { state, flags: 0, context_epoch: snapshot.context_epoch, output_epoch: 1, error: native_error };
        Ok(OK)
    })
}
unsafe extern "C" fn release(owner: *mut c_void) {
    if !owner.is_null() { unsafe { drop(Arc::from_raw(owner.cast::<Frame>())); } }
}
unsafe extern "C" fn frame(session: Session, known: FrameIdentity, output: *mut FrameSurface, error: *mut Error) -> Status {
    boundary(error, "RFVP_FRAME_FAILED", || {
        let handle = unsafe { handle(session)? };
        if handle.native.gpu_delivery {
            return Ok(failure(error, UNSUPPORTED, "RFVP_GPU_ACQUIRE_REQUIRED", "GPU sessions require the negotiated D3D12 acquire function"));
        }
        let output = unsafe { output.as_mut() }.ok_or_else(|| anyhow::anyhow!("missing frame output"))?;
        let snapshot = handle.native.snapshot();
        let identity = FrameIdentity { session_epoch: handle.epoch, output_epoch: 1,
            generation: snapshot.frame.as_ref().map_or(0, |frame| frame.generation) };
        let mut result = FrameSurface { state: FRAME_PENDING, reserved: 0, identity, timestamp_ns: 0,
            info: surface(&handle.native), cpu: CpuLease { pixels: Bytes::EMPTY, owner: std::ptr::null_mut(), release: None } };
        if let Some(frame) = snapshot.frame.as_ref() {
            result.timestamp_ns = frame.timestamp_ns;
            if known == identity { result.state = FRAME_UNCHANGED; }
            else {
                result.state = FRAME_READY;
                result.cpu = CpuLease { pixels: Bytes::new(&frame.pixels), owner: Arc::into_raw(frame.clone()).cast_mut().cast(), release: Some(release) };
            }
        }
        *output = result;
        Ok(OK)
    })
}
unsafe extern "C" fn cancel(session: Session) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Ok(handle) = unsafe { handle(session) } { handle.native.cancel(); }
    }));
}
unsafe extern "C" fn close(session: Session, error: *mut Error) -> Status {
    boundary(error, "RFVP_CLOSE_FAILED", || {
        anyhow::ensure!(!session.is_null() && session.cast::<Handle>().is_aligned(), "RFVP_SESSION_INVALID");
        let handle = unsafe { Box::from_raw(session.cast::<Handle>()) };
        let result = handle.native.close();
        drop(handle);
        result?;
        Ok(OK)
    })
}
unsafe extern "C" fn extension(name: Bytes, version: u32, output: *mut *const c_void) -> Status {
    if output.is_null() { return INVALID_ARGUMENT; }
    unsafe { *output = std::ptr::null(); }
    #[cfg(windows)]
    if version == D3D12_EXTENSION_VERSION && unsafe { name.as_slice(128) }.is_ok_and(|name| name == D3D12_EXTENSION) {
        unsafe { *output = (&d3d12::API as *const D3D12Api).cast(); }
        return OK;
    }
    UNSUPPORTED
}

static API: CoreApi = CoreApi { header: Header::of::<CoreApi>(), initialize: Some(initialize), describe: Some(describe),
    probe: Some(probe), open: Some(open), input: Some(input), control: Some(control), state: Some(state), frame: Some(frame),
    cancel: Some(cancel), close: Some(close), extension: Some(extension) };

/// Dynamic and static registration use the same table and lifecycle.
/// # Safety
/// `output` is writable storage for a single table pointer.
#[cfg_attr(feature = "native-dylib-export", no_mangle)]
pub unsafe extern "C" fn astra_emu_query(version: u32, output: *mut *const CoreApi) -> Status {
    if output.is_null() { return INVALID_ARGUMENT; }
    unsafe { *output = std::ptr::null(); }
    if version != ABI_VERSION { return UNSUPPORTED; }
    unsafe { *output = &API; }
    OK
}
