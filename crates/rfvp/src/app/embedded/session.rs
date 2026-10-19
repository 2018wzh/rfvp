use super::{Delivery, Duration, EmbeddedApp, Nls, Result};
use parking_lot::Mutex;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc::{self, SyncSender, TrySendError}};
use std::thread::{self, JoinHandle};
use std::time::Instant;
use winit::{event::WindowEvent, keyboard::{Key, PhysicalKey}};

pub struct Frame {
    pub pixels: Vec<u8>,
    pub generation: u64,
    pub timestamp_ns: u64,
    #[cfg(windows)]
    pub gpu: Option<super::d3d12::GpuFrame>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RunState { Running, Paused, Waiting, Finished, Failed }

pub struct Snapshot {
    pub state: RunState,
    pub context_epoch: u64,
    pub error: Option<String>,
    pub frame: Option<Arc<Frame>>,
}

pub enum Input {
    Key { physical: PhysicalKey, logical: Key, pressed: bool, repeat: bool },
    Window(WindowEvent),
}

enum Command { Input { events: Vec<Input>, suspended: Option<bool> }, Pause(bool) }

pub struct PointerRequest {
    pub position: Option<(i32, i32)>,
    pub visible: Option<bool>,
}

pub trait Platform: Send + Sync {
    fn window_mode(&self, mode: i32, logical: (u32, u32), context_epoch: u64) -> Result<()>;
    fn pointer(&self, request: PointerRequest, context_epoch: u64) -> Result<()>;
    /// Cancel pending requests and publish the next live context epoch to the Host.
    fn invalidate(&self, next_context_epoch: u64);
    fn is_cancelled(&self) -> bool;
}

/// The platform thread only enqueues control/input and borrows completed frames.
/// App, wgpu and native VM synchronization live exclusively on `rfvp-embedded`.
pub struct NativeSession {
    commands: SyncSender<Command>,
    cancellation: Arc<AtomicBool>,
    snapshot: Arc<Mutex<Snapshot>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    waker: thread::Thread,
    platform: Arc<dyn Platform>,
    pub logical: (u32, u32),
    pub raster: (u32, u32),
    pub gpu_delivery: bool,
}

impl NativeSession {
    pub fn open(root: String, nls: Nls, scale: f64, translation_table: String, subtitle_map: String, platform: Arc<dyn Platform>, initially_suspended: bool) -> Result<Self> {
        Self::open_with_delivery(root, nls, scale, translation_table, subtitle_map, platform, initially_suspended, Delivery::Cpu)
    }

    pub fn open_with_delivery(root: String, nls: Nls, scale: f64, translation_table: String, subtitle_map: String, platform: Arc<dyn Platform>,
        initially_suspended: bool, delivery: Delivery) -> Result<Self> {
        #[cfg(windows)]
        let gpu_delivery = matches!(delivery, Delivery::D3D12(_));
        #[cfg(not(windows))]
        let gpu_delivery = false;
        let (commands, receiver) = mpsc::sync_channel(256);
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);
        let cancellation = Arc::new(AtomicBool::new(platform.is_cancelled()));
        let snapshot = Arc::new(Mutex::new(Snapshot {
            state: RunState::Running, context_epoch: 1, error: None, frame: None,
        }));
        let worker_cancel = cancellation.clone();
        let worker_snapshot = snapshot.clone();
        let worker_platform = platform.clone();
        let worker = thread::Builder::new().name("rfvp-embedded".into()).stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                    let mut app = match EmbeddedApp::open_with_delivery(&root, nls, scale, &translation_table, &subtitle_map, worker_cancel.clone(), delivery) {
                        Ok(app) => app,
                        Err(error) => {
                            let _ = startup_tx.send(Err(format!("{error:#}")));
                            return Err(error);
                        }
                    };
                    let logical = app.logical_size();
                    let raster = app.raster_size();
                    if initially_suspended {
                        app.set_paused(true)?;
                        worker_snapshot.lock().state = RunState::Paused;
                    }
                    let _ = startup_tx.send(Ok((logical, raster)));
                    let mut pool: Option<[Arc<Frame>; 3]> = (!gpu_delivery).then(|| std::array::from_fn(|_| Arc::new(Frame {
                        pixels: Vec::with_capacity(raster.0 as usize * raster.1 as usize * 4),
                        generation: 0, timestamp_ns: 0,
                        #[cfg(windows)]
                        gpu: None,
                    })));
                    let mut capture: Option<usize> = None;
                    let mut generation = 0u64;
                    let mut suspended = initially_suspended;
                    let mut manual_pause = false;
                    let mut paused = suspended;
                    let origin = Instant::now();
                    let mut previous = origin;
                    let mut deadline = origin;
                    // Native desktop presentation uses FIFO; the windowless endpoint owns
                    // that pacing rather than asking the Manager to advance a game clock.
                    let cadence = Duration::from_nanos(1_000_000_000 / 60);
                    let run_result = (|| -> Result<()> {
                    while !worker_cancel.load(Ordering::Acquire) {
                        if worker_platform.is_cancelled() {
                            worker_cancel.store(true, Ordering::Release);
                            break;
                        }
                        // Bound a batch so a busy input producer cannot starve native ticks.
                        for _ in 0..256 {
                            let Ok(command) = receiver.try_recv() else { break };
                            match command {
                                Command::Input { events, suspended: window_suspended } => {
                                    for input in events {
                                        match input {
                                            Input::Key { physical, logical, pressed, repeat } =>
                                                app.input_key(physical, logical, pressed, repeat),
                                            Input::Window(event) => app.input_window_event(&event),
                                        }
                                    }
                                    if let Some(value) = window_suspended { suspended = value; }
                                }
                                Command::Pause(value) => manual_pause = value,
                            }
                            let next_paused = manual_pause || suspended;
                            if next_paused != paused {
                                app.set_paused(next_paused)?;
                                paused = next_paused;
                                previous = Instant::now();
                                deadline = previous;
                                worker_snapshot.lock().state = if paused { RunState::Paused } else { RunState::Running };
                            }
                        }
                        if let Some(slot) = capture {
                            let pool = pool.as_mut().expect("CPU capture has a CPU pool");
                            let frame = Arc::get_mut(&mut pool[slot]).expect("capture slot is private");
                            if app.poll_capture(&mut frame.pixels)? {
                                generation = generation.checked_add(1).ok_or_else(|| anyhow::anyhow!("RFVP_FRAME_GENERATION_EXHAUSTED"))?;
                                frame.generation = generation;
                                worker_snapshot.lock().frame = Some(pool[slot].clone());
                                capture = None;
                            }
                        }
                        let now = Instant::now();
                        if !paused && now >= deadline {
                            let exited = app.step(now.duration_since(previous))?;
                            previous = now;
                            let epoch = app.context_epoch();
                            let old_epoch = worker_snapshot.lock().context_epoch;
                            if epoch != old_epoch {
                                worker_platform.invalidate(epoch);
                                app.cancel_capture();
                                capture = None;
                                worker_snapshot.lock().context_epoch = epoch;
                            }
                            let mode = app.take_window_mode();
                            let pointer = app.take_pointer();
                            if mode.is_some() || pointer.is_some() {
                                app.set_paused(true)?;
                                worker_snapshot.lock().state = RunState::Waiting;
                                if let Some(mode) = mode { worker_platform.window_mode(mode, logical, epoch)?; }
                                if let Some(pointer) = pointer { worker_platform.pointer(pointer, epoch)?; }
                                app.set_paused(false)?;
                                previous = Instant::now();
                                worker_snapshot.lock().state = RunState::Running;
                            }
                            deadline = previous + cadence;
                            #[cfg(windows)]
                            if gpu_delivery {
                                let next = generation.checked_add(1).ok_or_else(|| anyhow::anyhow!("RFVP_FRAME_GENERATION_EXHAUSTED"))?;
                                let timestamp_ns = now.duration_since(origin).as_nanos().min(u64::MAX as u128) as u64;
                                if let Some(frame) = app.publish_gpu(next, timestamp_ns)? {
                                    generation = next;
                                    worker_snapshot.lock().frame = Some(frame);
                                }
                            }
                            if !gpu_delivery && capture.is_none() {
                                let pool = pool.as_mut().expect("CPU delivery has a CPU pool");
                                if let Some(slot) = pool.iter().position(|frame| Arc::strong_count(frame) == 1) {
                                    Arc::get_mut(&mut pool[slot]).expect("unleased frame").timestamp_ns =
                                        now.duration_since(origin).as_nanos().min(u64::MAX as u128) as u64;
                                    app.begin_capture()?;
                                    capture = Some(slot);
                                }
                                // A full lease pool drops only display publications, never VM/audio work.
                            }
                            if exited {
                                worker_snapshot.lock().state = RunState::Finished;
                                break;
                            }
                        }
                        let wait = if capture.is_some() { Duration::from_millis(1) }
                            else if paused { Duration::from_millis(100) }
                            else { deadline.saturating_duration_since(Instant::now()) };
                        thread::park_timeout(wait);
                    }
                    Ok(())
                    })();
                    // Shutdown also cancels the VM. Preserve whether cancellation
                    // actually caused the request failure before initiating cleanup.
                    let cancelled = worker_cancel.load(Ordering::Acquire);
                    let shutdown_result = app.shutdown();
                    let run_result = match run_result {
                        Err(error) if cancelled && error.to_string() == "RFVP_SESSION_CANCELLED" => Ok(()),
                        other => other,
                    };
                    run_result.and(shutdown_result)
                }));
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(format!("{error:#}")),
                    Err(_) => Some("RFVP_NATIVE_WORKER_PANICKED".into()),
                };
                if let Some(error) = error {
                    log::error!("{error}");
                    let mut state = worker_snapshot.lock();
                    state.error = Some(error);
                    state.state = RunState::Failed;
                }
            })?;
        let startup = loop {
            match startup_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(result) => {
                    if platform.is_cancelled() {
                        break Ok(Err("RFVP_SESSION_CANCELLED".to_owned()));
                    }
                    break Ok(result);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if platform.is_cancelled() {
                        cancellation.store(true, Ordering::Release);
                        platform.invalidate(1);
                        worker.thread().unpark();
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break Err(()),
            }
        };
        match startup {
            Ok(Ok((logical, raster))) => Ok(Self {
                commands, cancellation, snapshot, waker: worker.thread().clone(),
                worker: Mutex::new(Some(worker)), platform, logical, raster, gpu_delivery,
            }),
            outcome => {
                cancellation.store(true, Ordering::Release);
                worker.thread().unpark();
                let joined = worker.join();
                let message = match outcome {
                    Ok(Err(message)) => message,
                    _ if joined.is_err() => "RFVP_NATIVE_STARTUP_PANICKED".into(),
                    _ => snapshot.lock().error.clone().unwrap_or_else(|| "RFVP_NATIVE_STARTUP_FAILED".into()),
                };
                Err(anyhow::anyhow!(message))
            }
        }
    }

    fn enqueue(&self, command: Command) -> Result<()> {
        anyhow::ensure!(!self.cancellation.load(Ordering::Acquire), "RFVP_SESSION_CANCELLED");
        anyhow::ensure!(!matches!(self.snapshot.lock().state, RunState::Failed | RunState::Finished), "RFVP_SESSION_NOT_RUNNING");
        self.commands.try_send(command).map_err(|error| match error {
            TrySendError::Full(_) => anyhow::anyhow!("RFVP_INPUT_CAPACITY"),
            TrySendError::Disconnected(_) => anyhow::anyhow!("RFVP_SESSION_CLOSED"),
        })?;
        self.waker.unpark();
        Ok(())
    }

    pub fn input(&self, events: Vec<Input>, suspended: Option<bool>) -> Result<()> {
        self.enqueue(Command::Input { events, suspended })
    }
    pub fn set_paused(&self, paused: bool) -> Result<()> { self.enqueue(Command::Pause(paused)) }
    pub fn snapshot(&self) -> parking_lot::MutexGuard<'_, Snapshot> { self.snapshot.lock() }
    pub fn cancel(&self) {
        self.cancellation.store(true, Ordering::Release);
        let context_epoch = self.snapshot.lock().context_epoch;
        self.platform.invalidate(context_epoch);
        self.waker.unpark();
    }
    pub fn close(&self) -> Result<()> {
        self.cancel();
        let joined = self.worker.lock().take().map(|worker| worker.join());
        anyhow::ensure!(!matches!(joined, Some(Err(_))), "RFVP_NATIVE_JOIN_PANICKED");
        if let Some(error) = self.snapshot.lock().error.as_ref() { anyhow::bail!("{error}"); }
        Ok(())
    }
}

impl Drop for NativeSession {
    fn drop(&mut self) {
        if let Err(error) = self.close() { log::error!("native session close: {error:#}"); }
    }
}
