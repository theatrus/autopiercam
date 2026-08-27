use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, TryRecvError},
    },
    thread,
    time::Duration,
};

use autopiercam::{AgentControl, AgentMonitor, run_agent_with_monitor};
use autopiercam_asi::Sdk;
use autopiercam_protocol::AgentStatus;

#[derive(Clone, Debug)]
pub(crate) struct WorkerOptions {
    pub(crate) config_path: PathBuf,
    pub(crate) sdk_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum TrayCommand {
    SetPaused(bool),
    CaptureNow,
    Shutdown,
}

#[derive(Debug)]
pub(crate) enum WorkerEvent {
    StatusChanged(AgentStatus),
    WorkerStopped,
}

#[derive(Clone)]
pub(crate) struct WorkerClient {
    control: AgentControl,
    monitor: AgentMonitor,
    stopped: Arc<AtomicBool>,
}

impl WorkerClient {
    pub(crate) fn send(&self, command: TrayCommand) -> Result<(), WorkerStopped> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(WorkerStopped);
        }
        match command {
            TrayCommand::SetPaused(true) => self.control.pause(),
            TrayCommand::SetPaused(false) => self.control.resume(),
            TrayCommand::CaptureNow => self.control.capture_now(),
            TrayCommand::Shutdown => {
                self.monitor.mark_stopping();
                self.control.shutdown();
            }
        }
        Ok(())
    }

    pub(crate) fn monitor(&self) -> AgentMonitor {
        self.monitor.clone()
    }

    pub(crate) fn control(&self) -> AgentControl {
        self.control.clone()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WorkerStopped;

impl fmt::Display for WorkerStopped {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("capture worker has already stopped")
    }
}

impl std::error::Error for WorkerStopped {}

/// Starts a supervisor around the camera-owning thread. A startup fault remains
/// inspectable through the tray and IPC until the user explicitly quits.
pub(crate) fn start_capture_worker<F>(
    options: WorkerOptions,
    emit: F,
) -> std::io::Result<WorkerClient>
where
    F: Fn(WorkerEvent) + Send + 'static,
{
    let control = AgentControl::new();
    let monitor = AgentMonitor::new();
    let stopped = Arc::new(AtomicBool::new(false));

    let supervisor_control = control.clone();
    let supervisor_monitor = monitor.clone();
    let supervisor_stopped = Arc::clone(&stopped);
    thread::Builder::new()
        .name("autopiercam-supervisor".to_owned())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                supervise_camera(options, &supervisor_control, &supervisor_monitor, &emit);
            }));
            if result.is_err() {
                supervisor_monitor.report_fault("capture supervisor panicked");
                emit(WorkerEvent::StatusChanged(supervisor_monitor.snapshot()));
            }
            supervisor_stopped.store(true, Ordering::Release);
            emit(WorkerEvent::WorkerStopped);
        })?;

    Ok(WorkerClient {
        control,
        monitor,
        stopped,
    })
}

fn supervise_camera<F>(
    options: WorkerOptions,
    control: &AgentControl,
    monitor: &AgentMonitor,
    emit: &F,
) where
    F: Fn(WorkerEvent),
{
    let camera_control = control.clone();
    let camera_monitor = monitor.clone();
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let camera_thread = thread::Builder::new()
        .name("autopiercam-camera".to_owned())
        .spawn(move || {
            let result = run_camera(options, &camera_control, &camera_monitor);
            if let Err(error) = &result {
                camera_monitor.report_fault(error.clone());
            }
            let _ = finished_tx.send(result);
        });

    let camera_thread = match camera_thread {
        Ok(handle) => handle,
        Err(error) => {
            monitor.report_fault(format!("failed to start camera thread: {error}"));
            emit(WorkerEvent::StatusChanged(monitor.snapshot()));
            wait_for_shutdown(control);
            return;
        }
    };

    let mut last_status = None;
    let camera_result = loop {
        let status = monitor.snapshot();
        if last_status.as_ref() != Some(&status) {
            emit(WorkerEvent::StatusChanged(status.clone()));
            last_status = Some(status);
        }

        match finished_rx.try_recv() {
            Ok(result) => break Some(result),
            Err(TryRecvError::Disconnected) => break None,
            Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(100)),
        }
    };

    if camera_thread.join().is_err() {
        monitor.report_fault("camera owner thread panicked");
    } else if let Some(Err(error)) = camera_result {
        monitor.report_fault(error);
    }
    emit(WorkerEvent::StatusChanged(monitor.snapshot()));

    if !control.is_shutdown() {
        wait_for_shutdown(control);
    }
    monitor.mark_stopping();
    emit(WorkerEvent::StatusChanged(monitor.snapshot()));
}

fn run_camera(
    options: WorkerOptions,
    control: &AgentControl,
    monitor: &AgentMonitor,
) -> Result<(), String> {
    let sdk = match options.sdk_path {
        Some(path) => Sdk::load(&path),
        None => Sdk::load_default(),
    }
    .map(Arc::new)
    .map_err(|error| format!("loading ZWO ASI SDK: {error}"))?;

    run_agent_with_monitor(&sdk, &options.config_path, None, control, monitor)
        .map_err(|error| format!("capture worker failed: {error:#}"))
}

fn wait_for_shutdown(control: &AgentControl) {
    while !control.is_shutdown() {
        thread::sleep(Duration::from_millis(100));
    }
}
