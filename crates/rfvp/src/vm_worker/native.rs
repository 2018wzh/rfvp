use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, RwLock};
use std::thread;

use crate::script::parser::Parser;
use crate::subsystem::resources::thread_manager::ThreadManager;
use crate::subsystem::world::GameData;
use crate::vm_runner::{VmRunner, VmTickReport};

/// Events delivered from the main thread to the VM thread.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// Advance the script VM by one frame worth of time.
    Frame { frame_ms: u64 },

    /// Same as `Frame`, but the sender waits until the VM tick completes.
    FrameSync {
        frame_ms: u64,
        done: Sender<VmTickReport>,
    },

    /// Notify the VM that a global dissolve has finished.
    ///
    /// This is used to unblock contexts waiting on DISSOLVE_WAIT without
    /// having to wait for the next frame event. The VM will run a zero-delta
    /// tick to process state transitions.
    DissolveDone,

    /// Same as `DissolveDone`, but the sender waits until the VM tick completes.
    DissolveDoneSync { done: Sender<()> },

    /// Notify the VM that an input event was received on the main thread.
    ///
    /// We do not carry payload here: the main thread already recorded the
    /// input into `GameData.inputs_manager` while holding the same write lock.
    /// This event exists to wake the VM so scripts can react immediately
    /// without waiting for the next frame.
    InputSignal,

    /// Terminate the VM thread.
    Stop,
}

/// A lightweight VM worker that runs the coroutine-based VM on a dedicated OS thread.
///
/// Design constraints:
/// - The VM thread must never touch wgpu objects (Device/Queue/Surface/Textures).
/// - All state mutations happen under a shared RwLock<Box<GameData>>.
///   The main thread should send Frame events from `AboutToWait` so the VM runs
///   while the event loop is idle and does not block rendering.
pub struct VmWorker {
    tx: Sender<EngineEvent>,
    input_signal_pending: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    cancellation: Arc<AtomicBool>,
    timeline_epoch: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
}

impl VmWorker {
    pub fn spawn(
        game_data: Arc<RwLock<Box<GameData>>>,
        parser: Parser,
        script_engine: ThreadManager,
    ) -> Self {
        Self::spawn_with_cancellation(game_data, parser, script_engine, Arc::new(AtomicBool::new(false)))
    }

    pub fn spawn_with_cancellation(
        game_data: Arc<RwLock<Box<GameData>>>,
        mut parser: Parser,
        script_engine: ThreadManager,
        cancellation: Arc<AtomicBool>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<EngineEvent>();
        let input_signal_pending = Arc::new(AtomicBool::new(false));
        let worker_input_signal_pending = Arc::clone(&input_signal_pending);
        let timeline_epoch = Arc::new(AtomicU64::new(1));
        let worker_epoch = timeline_epoch.clone();
        let failed = Arc::new(AtomicBool::new(false));
        let worker_failed = failed.clone();
        let worker_cancel = cancellation.clone();

        let worker = thread::Builder::new()
            .name("rfvp-vm".to_string())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let mut vm = VmRunner::new(script_engine);
                vm.set_cancellation(worker_cancel.clone());

                while let Ok(ev) = rx.recv() {
                    if worker_cancel.load(Ordering::Acquire) { break; }
                    let (frame_ms, frame_done, dissolve_done, phase) = match ev {
                        EngineEvent::Stop => break,
                        EngineEvent::Frame { frame_ms } => (frame_ms, None, None, "frame"),
                        EngineEvent::FrameSync { frame_ms, done } => (frame_ms, Some(done), None, "frame"),
                        EngineEvent::DissolveDone => (0, None, None, "dissolve done"),
                        EngineEvent::DissolveDoneSync { done } => (0, None, Some(done), "dissolve done"),
                        EngineEvent::InputSignal => {
                            worker_input_signal_pending.store(false, Ordering::Release);
                            (0, None, None, "input signal")
                        }
                    };
                    let mut gd = match game_data.write() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    let mut report = VmTickReport::default();
                    if !gd.get_halt() {
                        match vm.tick(&mut **gd, &mut parser, frame_ms) {
                            Ok(completed) => {
                                if completed.timeline_reset { worker_epoch.fetch_add(1, Ordering::AcqRel); }
                                report = completed;
                            }
                            Err(error) => {
                                log::error!("VM thread tick error ({phase}): {error:?}");
                                worker_failed.store(true, Ordering::Release);
                            }
                        }
                    }
                    if let Some(done) = frame_done { let _ = done.send(report); }
                    if let Some(done) = dissolve_done { let _ = done.send(()); }
                }
            })
            .expect("failed to spawn VM thread");

        Self {
            tx,
            input_signal_pending,
            worker: Some(worker),
            cancellation,
            timeline_epoch,
            failed,
        }
    }

    pub fn timeline_epoch(&self) -> u64 { self.timeline_epoch.load(Ordering::Acquire) }
    pub fn has_failed(&self) -> bool { self.failed.load(Ordering::Acquire) }

    /// Stop and join before the owning App releases audio, saves or global state.
    pub fn shutdown(&mut self) {
        self.cancellation.store(true, Ordering::Release);
        let _ = self.tx.send(EngineEvent::Stop);
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                log::error!("RFVP VM worker panicked during shutdown");
            }
        }
    }

    #[inline]
    pub fn send_frame_ms(&self, frame_ms: u64) {
        // Ignore send errors during shutdown.
        let _ = self.tx.send(EngineEvent::Frame { frame_ms });
    }

    #[inline]
    pub fn send_dissolve_done(&self) {
        let _ = self.tx.send(EngineEvent::DissolveDone);
    }

    #[inline]
    pub fn send_input_signal(&self) {
        if self.input_signal_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.tx.send(EngineEvent::InputSignal).is_err() {
            self.input_signal_pending.store(false, Ordering::Release);
        }
    }

    #[inline]
    pub fn send_frame_ms_sync(&self, frame_ms: u64) -> VmTickReport {
        let (done_tx, done_rx) = mpsc::channel::<VmTickReport>();
        let _ = self.tx.send(EngineEvent::FrameSync {
            frame_ms,
            done: done_tx,
        });
        match done_rx.recv() {
            Ok(report) => report,
            Err(_) => {
                self.failed.store(true, Ordering::Release);
                VmTickReport::default()
            }
        }
    }

    #[inline]
    pub fn send_dissolve_done_sync(&self) {
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let _ = self
            .tx
            .send(EngineEvent::DissolveDoneSync { done: done_tx });
        if done_rx.recv().is_err() {
            self.failed.store(true, Ordering::Release);
        }
    }
}

impl Drop for VmWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}
