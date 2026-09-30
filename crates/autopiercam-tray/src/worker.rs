use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use autopiercam::{AgentControl, AgentMonitor, PreviewHub, run_agent_with_monitor_and_preview};
use autopiercam_camera::Driver;
use autopiercam_protocol::{AgentState, AgentStatus};

const SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
const MAX_CAPTURE_REQUESTS_PER_POLL: u64 = 1_024;
const RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

#[derive(Clone, Debug)]
pub(crate) struct WorkerOptions {
    pub(crate) config_path: PathBuf,
    pub(crate) sdk_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrayCommand {
    SetPaused(bool),
    CaptureNow,
    Restart,
    ReloadConfiguration,
    Shutdown,
}

#[derive(Debug)]
pub(crate) enum WorkerEvent {
    StatusChanged(Box<AgentStatus>),
    WorkerStopped,
}

#[derive(Clone)]
pub(crate) struct WorkerClient {
    sharing: Option<autopiercam::SharingClient>,
    commands: Arc<Mutex<Sender<TrayCommand>>>,
    monitor: AgentMonitor,
    preview: PreviewHub,
    signals: Arc<WorkerSignals>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
}

#[derive(Debug, Default)]
struct WorkerSignals {
    sharing_paused: AtomicBool,
    sharing_min_sequence: AtomicU64,
    restart_pending: AtomicBool,
    start_admission: Mutex<()>,
    stopping: AtomicBool,
}

// Native SDK calls cannot be cancelled safely. Only an explicit whole-host
// shutdown permits this fallback; never abandon a camera owner and reopen a
// second handle in the same process. Keep this independent of the supervisor,
// which may itself be joining a stuck camera or draining another service.
fn shutdown_watchdog(signals: Weak<WorkerSignals>, grace: Duration, on_timeout: impl FnOnce()) {
    let mut started = None;
    loop {
        let Some(signals) = signals.upgrade() else {
            return;
        };
        if signals.stopping.load(Ordering::Acquire) {
            let since = started.get_or_insert_with(Instant::now);
            if since.elapsed() >= grace {
                on_timeout();
                return;
            }
        }
        drop(signals);
        thread::sleep(SUPERVISOR_POLL_INTERVAL);
    }
}

fn terminate_stuck_host() {
    // No logging/locking or DLL detach callbacks here: those can also be stuck.
    // SAFETY: This pseudo-handle targets only this process, after shutdown grace
    // has expired. TerminateProcess avoids ExitProcess's DLL-detach deadlocks.
    unsafe {
        windows_sys::Win32::System::Threading::TerminateProcess(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            1,
        );
    }
    std::process::abort(); // Only reached if self-termination unexpectedly failed.
}

impl WorkerClient {
    pub(crate) fn send(&self, command: TrayCommand) -> Result<(), WorkerStopped> {
        let _admission = self
            .signals
            .start_admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let commands = self
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.signals.stopping.load(Ordering::Acquire) {
            return Err(WorkerStopped);
        }
        if let TrayCommand::SetPaused(paused) = command {
            self.signals.sharing_paused.store(paused, Ordering::Release);
        }
        if !matches!(
            command,
            TrayCommand::CaptureNow | TrayCommand::ReloadConfiguration
        ) {
            let sequence = self
                .preview
                .snapshot()
                .frame
                .map_or(0, |frame| frame.metadata.sequence);
            self.signals
                .sharing_min_sequence
                .store(sequence, Ordering::Release);
            if let Some(sharing) = &self.sharing {
                sharing.invalidate();
            }
        }
        match command {
            TrayCommand::Restart if self.signals.restart_pending.swap(true, Ordering::AcqRel) => {
                return Ok(());
            }
            TrayCommand::Shutdown => {
                self.signals.stopping.store(true, Ordering::Release);
                tracing::info!(
                    "shutdown requested; allowing 30 seconds for cleanup before whole-process termination"
                );
            }
            TrayCommand::SetPaused(_)
            | TrayCommand::CaptureNow
            | TrayCommand::Restart
            | TrayCommand::ReloadConfiguration => {}
        }
        if commands.send(command).is_err() {
            if command == TrayCommand::Restart {
                self.signals.restart_pending.store(false, Ordering::Release);
            }
            self.signals.stopping.store(true, Ordering::Release);
            return Err(WorkerStopped);
        }
        Ok(())
    }

    pub(crate) fn monitor(&self) -> AgentMonitor {
        self.monitor.clone()
    }

    pub(crate) fn sharing(&self) -> Option<autopiercam::SharingClient> {
        self.sharing.clone()
    }

    pub(crate) fn preview(&self) -> PreviewHub {
        self.preview.clone()
    }

    pub(crate) fn join(&self) -> std::io::Result<()> {
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(thread) = thread else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_| std::io::Error::other("capture supervisor thread panicked"))
    }

    pub(crate) fn shutdown_and_join(&self) -> std::io::Result<()> {
        let _ = self.send(TrayCommand::Shutdown);
        self.join()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WorkerStopped;

impl fmt::Display for WorkerStopped {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("capture worker is stopping or has already stopped")
    }
}

impl std::error::Error for WorkerStopped {}

/// Starts a restartable supervisor around exactly one camera-owning thread.
/// Camera faults remain visible while the supervisor automatically reconnects.
pub(crate) fn start_capture_worker<F>(
    options: WorkerOptions,
    emit: F,
) -> std::io::Result<WorkerClient>
where
    F: Fn(WorkerEvent) + Send + 'static,
{
    let (commands, receiver) = mpsc::channel();
    let monitor = AgentMonitor::new();
    let preview = PreviewHub::new();
    let signals = Arc::new(WorkerSignals::default());
    let shutdown_signals = Arc::downgrade(&signals);
    thread::Builder::new()
        .name("autopiercam-shutdown-watchdog".to_owned())
        .spawn(move || shutdown_watchdog(shutdown_signals, SHUTDOWN_GRACE, terminate_stuck_host))?;
    let sharing_preview = preview.clone();
    let sharing_monitor = monitor.clone();
    let sharing_signals = signals.clone();
    let sharing_service = autopiercam::SharingService::start(
        &options.config_path,
        Arc::new(move || {
            if sharing_signals.sharing_paused.load(Ordering::Acquire)
                || sharing_signals.stopping.load(Ordering::Acquire)
                || sharing_signals.restart_pending.load(Ordering::Acquire)
            {
                return None;
            }
            autopiercam::sharing_frame(&sharing_preview, &sharing_monitor).filter(|frame| {
                frame.sequence > sharing_signals.sharing_min_sequence.load(Ordering::Acquire)
            })
        }),
    )
    .map_err(
        |error| tracing::warn!(%error, "Chatstronomy disabled; local capture remains available"),
    )
    .ok();
    let sharing = sharing_service
        .as_ref()
        .map(autopiercam::SharingService::client);

    let supervisor_monitor = monitor.clone();
    let supervisor_preview = preview.clone();
    let supervisor_signals = Arc::clone(&signals);
    let thread = thread::Builder::new()
        .name("autopiercam-supervisor".to_owned())
        .spawn(move || {
            let _sharing_service = sharing_service;
            let mut last_status = None;
            let result = catch_unwind(AssertUnwindSafe(|| {
                supervise_camera(
                    options,
                    receiver,
                    &supervisor_signals,
                    &supervisor_monitor,
                    &supervisor_preview,
                    &emit,
                    &mut last_status,
                );
            }));
            if result.is_err() {
                // CameraSession's Drop implementation stops and joins an active owner while
                // unwind passes through supervise_camera, so even this path leaves no detached
                // SDK thread.
                supervisor_monitor.report_fault("capture supervisor panicked");
                publish_status_if_changed(&supervisor_monitor, &emit, &mut last_status);
            }
            supervisor_signals.stopping.store(true, Ordering::Release);
            emit(WorkerEvent::WorkerStopped);
        })?;

    Ok(WorkerClient {
        sharing,
        commands: Arc::new(Mutex::new(commands)),
        monitor,
        preview,
        signals,
        thread: Arc::new(Mutex::new(Some(thread))),
    })
}

fn supervise_camera<F>(
    options: WorkerOptions,
    commands: Receiver<TrayCommand>,
    signals: &WorkerSignals,
    monitor: &AgentMonitor,
    preview: &PreviewHub,
    emit: &F,
    last_status: &mut Option<AgentStatus>,
) where
    F: Fn(WorkerEvent),
{
    let mut session = None;
    let mut intent = SupervisorIntent::new(false);
    let mut backoff = RetryBackoff::default();
    let mut retry_at = Instant::now();

    loop {
        if signals.stopping.load(Ordering::Acquire) {
            orderly_shutdown(&mut session, monitor, emit, last_status);
            return;
        }
        observe_session_status(
            monitor,
            emit,
            last_status,
            &mut session,
            &mut intent,
            &mut backoff,
        );

        if session.as_ref().is_some_and(CameraSession::is_finished) {
            let finished = session.take().expect("finished session was present");
            let reached_capturing = finished.reached_capturing;
            let restarting = finished.stop_requested_at.is_some();
            let outcome = finished.join_finished();
            if restarting {
                report_controlled_exit(monitor, outcome);
                monitor.mark_stopping();
                signals.restart_pending.store(false, Ordering::Release);
            } else {
                report_unexpected_exit(monitor, outcome);
                intent.faulted = true;
            }
            publish_status_if_changed(monitor, emit, last_status);
            if reached_capturing {
                backoff.reset();
            }
            retry_at = Instant::now()
                + if restarting {
                    Duration::ZERO
                } else {
                    backoff.next_delay()
                };
            continue;
        }

        // Drain an already queued lifecycle command before starting a due retry. This prevents
        // a queued Quit from briefly opening a new camera after Restart finished joining.
        match commands.try_recv() {
            Ok(command) => {
                if handle_command(
                    command,
                    &mut session,
                    &mut intent,
                    &mut backoff,
                    &mut retry_at,
                    &signals.restart_pending,
                    monitor,
                    emit,
                    last_status,
                ) {
                    return;
                }
                continue;
            }
            Err(TryRecvError::Disconnected) => {
                signals.stopping.store(true, Ordering::Release);
                orderly_shutdown(&mut session, monitor, emit, last_status);
                return;
            }
            Err(TryRecvError::Empty) => {}
        }

        if session.is_none() && !intent.faulted && Instant::now() >= retry_at {
            let _admission = signals
                .start_admission
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if signals.stopping.load(Ordering::Acquire) {
                continue;
            }
            match CameraSession::start(&options, monitor, preview, intent.paused) {
                Ok(camera) => session = Some(camera),
                Err(error) => {
                    intent.faulted = true;
                    monitor.report_fault(format!("failed to start camera thread: {error}"));
                    publish_status_if_changed(monitor, emit, last_status);
                    retry_at = Instant::now() + backoff.next_delay();
                }
            }
            continue;
        }

        let wait = if intent.faulted {
            SUPERVISOR_POLL_INTERVAL
        } else {
            session
                .as_ref()
                .map(|_| SUPERVISOR_POLL_INTERVAL)
                .unwrap_or_else(|| {
                    retry_at
                        .saturating_duration_since(Instant::now())
                        .min(SUPERVISOR_POLL_INTERVAL)
                })
        };
        match commands.recv_timeout(wait) {
            Ok(command) => {
                if handle_command(
                    command,
                    &mut session,
                    &mut intent,
                    &mut backoff,
                    &mut retry_at,
                    &signals.restart_pending,
                    monitor,
                    emit,
                    last_status,
                ) {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                signals.stopping.store(true, Ordering::Release);
                orderly_shutdown(&mut session, monitor, emit, last_status);
                return;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_command<F>(
    command: TrayCommand,
    session: &mut Option<CameraSession>,
    intent: &mut SupervisorIntent,
    backoff: &mut RetryBackoff,
    retry_at: &mut Instant,
    restart_pending: &AtomicBool,
    monitor: &AgentMonitor,
    emit: &F,
    last_status: &mut Option<AgentStatus>,
) -> bool
where
    F: Fn(WorkerEvent),
{
    let status = monitor.snapshot();
    let session_ready = matches!(status.state, AgentState::Capturing | AgentState::Paused)
        && session.as_ref().is_some_and(|camera| {
            camera.ready && camera.stop_requested_at.is_none() && !camera.is_finished()
        });
    let lifecycle = intent.accept(command, session_ready);

    match command {
        TrayCommand::SetPaused(paused) => {
            if let Some(camera) = session {
                camera.set_paused(paused);
            }
        }
        TrayCommand::CaptureNow if session_ready => {
            if let Some(camera) = session {
                camera.capture_now();
            }
        }
        TrayCommand::ReloadConfiguration => {
            if let Some(camera) = session {
                camera.control.reload_configuration();
            }
        }
        TrayCommand::CaptureNow | TrayCommand::Restart | TrayCommand::Shutdown => {}
    }

    match lifecycle {
        LifecycleRequest::None => false,
        LifecycleRequest::Restart => {
            restart_session(session, monitor, emit, last_status);
            // Keep sharing fenced and repeated restart requests coalesced while
            // the original owner stops, not merely until command dequeue.
            if session.is_none() {
                restart_pending.store(false, Ordering::Release);
            }
            backoff.reset();
            *retry_at = Instant::now();
            false
        }
        LifecycleRequest::Shutdown => {
            orderly_shutdown(session, monitor, emit, last_status);
            true
        }
    }
}

fn observe_session_status<F>(
    monitor: &AgentMonitor,
    emit: &F,
    last_status: &mut Option<AgentStatus>,
    session: &mut Option<CameraSession>,
    intent: &mut SupervisorIntent,
    backoff: &mut RetryBackoff,
) where
    F: Fn(WorkerEvent),
{
    if let Some(camera) = session.as_ref()
        && let Some(started) = camera.stop_requested_at
    {
        if started.elapsed() >= SHUTDOWN_GRACE {
            monitor.report_fault("Camera worker did not stop for restart. Quit and relaunch the AutoPierCam tray agent; no replacement camera handle will be opened while the old worker is running.");
        } else {
            monitor.mark_stopping();
        }
        publish_status_if_changed(monitor, emit, last_status);
        return;
    }
    let status = monitor.snapshot();
    if let Some(camera) = session {
        if monitor.capturing_generation() != camera.started_capturing_generation {
            camera.reached_capturing = true;
            backoff.reset();
        }
        match status.state {
            AgentState::Capturing => {
                camera.ready = true;
            }
            AgentState::Paused => camera.ready = true,
            AgentState::Starting
            | AgentState::Idle
            | AgentState::Faulted
            | AgentState::Stopping => camera.ready = false,
        }
        if camera.ready {
            let pending = intent.take_pending_captures(MAX_CAPTURE_REQUESTS_PER_POLL);
            for _ in 0..pending {
                camera.capture_now();
            }
        }
    }
    publish_snapshot_if_changed(status, emit, last_status);
}

fn restart_session<F>(
    session: &mut Option<CameraSession>,
    monitor: &AgentMonitor,
    emit: &F,
    last_status: &mut Option<AgentStatus>,
) where
    F: Fn(WorkerEvent),
{
    monitor.mark_stopping();
    publish_status_if_changed(monitor, emit, last_status);
    if let Some(camera) = session {
        camera.request_stop();
    }
}

fn orderly_shutdown<F>(
    session: &mut Option<CameraSession>,
    monitor: &AgentMonitor,
    emit: &F,
    last_status: &mut Option<AgentStatus>,
) where
    F: Fn(WorkerEvent),
{
    monitor.mark_stopping();
    publish_status_if_changed(monitor, emit, last_status);
    if let Some(camera) = session.take() {
        report_controlled_exit(monitor, camera.shutdown_and_join());
    }
    monitor.mark_stopping();
    publish_status_if_changed(monitor, emit, last_status);
}

fn report_unexpected_exit(monitor: &AgentMonitor, outcome: SessionExit) {
    match outcome {
        SessionExit::Completed => monitor.report_fault("camera session stopped unexpectedly"),
        SessionExit::Failed(error) => monitor.report_fault(error),
        SessionExit::Panicked => monitor.report_fault("camera owner thread panicked"),
    }
}

fn report_controlled_exit(monitor: &AgentMonitor, outcome: SessionExit) {
    match outcome {
        SessionExit::Completed => {}
        SessionExit::Failed(error) => monitor.report_fault(error),
        SessionExit::Panicked => {
            monitor.report_fault("camera owner thread panicked while stopping")
        }
    }
}

fn publish_status_if_changed<F>(
    monitor: &AgentMonitor,
    emit: &F,
    last_status: &mut Option<AgentStatus>,
) where
    F: Fn(WorkerEvent),
{
    publish_snapshot_if_changed(monitor.snapshot(), emit, last_status);
}

fn publish_snapshot_if_changed<F>(
    status: AgentStatus,
    emit: &F,
    last_status: &mut Option<AgentStatus>,
) where
    F: Fn(WorkerEvent),
{
    if last_status.as_ref() == Some(&status) {
        return;
    }
    if status.state == AgentState::Faulted
        && last_status.as_ref().is_none_or(|last| {
            last.state != AgentState::Faulted || last.last_error != status.last_error
        })
    {
        tracing::warn!(error = ?status.last_error, frames_captured = status.frames_captured,
            frames_saved = status.frames_saved, "capture worker fault");
    }
    emit(WorkerEvent::StatusChanged(Box::new(status.clone())));
    *last_status = Some(status);
}

struct CameraSession {
    control: AgentControl,
    thread: Option<JoinHandle<Result<(), String>>>,
    ready: bool,
    reached_capturing: bool,
    started_capturing_generation: u64,
    stop_requested_at: Option<Instant>,
}

impl CameraSession {
    fn start(
        options: &WorkerOptions,
        monitor: &AgentMonitor,
        preview: &PreviewHub,
        paused: bool,
    ) -> std::io::Result<Self> {
        let control = AgentControl::new();
        if paused {
            control.pause();
        }
        let camera_options = options.clone();
        let camera_control = control.clone();
        let camera_monitor = monitor.clone();
        let camera_preview = preview.clone();
        let started_capturing_generation = monitor.capturing_generation();
        let thread = thread::Builder::new()
            .name("autopiercam-camera".to_owned())
            .spawn(move || {
                run_camera(
                    camera_options,
                    &camera_control,
                    &camera_monitor,
                    &camera_preview,
                )
            })?;
        Ok(Self {
            control,
            thread: Some(thread),
            ready: false,
            reached_capturing: false,
            started_capturing_generation,
            stop_requested_at: None,
        })
    }

    fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn request_stop(&mut self) {
        self.stop_requested_at.get_or_insert_with(Instant::now);
        self.ready = false;
        self.control.shutdown();
    }

    fn set_paused(&self, paused: bool) {
        if paused {
            self.control.pause();
        } else {
            self.control.resume();
        }
    }

    fn capture_now(&self) {
        self.control.capture_now();
    }

    fn join_finished(mut self) -> SessionExit {
        self.join_inner()
    }

    fn shutdown_and_join(mut self) -> SessionExit {
        self.control.shutdown();
        self.join_inner()
    }

    fn join_inner(&mut self) -> SessionExit {
        let Some(thread) = self.thread.take() else {
            return SessionExit::Panicked;
        };
        match thread.join() {
            Ok(Ok(())) => SessionExit::Completed,
            Ok(Err(error)) => SessionExit::Failed(error),
            Err(_) => SessionExit::Panicked,
        }
    }
}

impl Drop for CameraSession {
    fn drop(&mut self) {
        // Never detach a live SDK owner and let another session open a handle.
        // This join can hang inside native code; the independent host watchdog
        // bounds an explicit Quit even when this cleanup cannot finish.
        self.control.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SessionExit {
    Completed,
    Failed(String),
    Panicked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifecycleRequest {
    None,
    Restart,
    Shutdown,
}

#[derive(Debug)]
struct SupervisorIntent {
    faulted: bool,
    paused: bool,
    pending_captures: u64,
}

impl SupervisorIntent {
    fn new(paused: bool) -> Self {
        Self {
            faulted: false,
            paused,
            pending_captures: 0,
        }
    }

    fn accept(&mut self, command: TrayCommand, session_ready: bool) -> LifecycleRequest {
        match command {
            TrayCommand::SetPaused(paused) => {
                self.paused = paused;
                LifecycleRequest::None
            }
            TrayCommand::CaptureNow => {
                if !session_ready {
                    self.pending_captures = self.pending_captures.saturating_add(1);
                }
                LifecycleRequest::None
            }
            TrayCommand::ReloadConfiguration if self.faulted => {
                self.faulted = false;
                LifecycleRequest::Restart
            }
            TrayCommand::ReloadConfiguration => LifecycleRequest::None,
            TrayCommand::Restart => {
                self.faulted = false;
                LifecycleRequest::Restart
            }
            TrayCommand::Shutdown => LifecycleRequest::Shutdown,
        }
    }

    fn take_pending_captures(&mut self, maximum: u64) -> u64 {
        let count = self.pending_captures.min(maximum);
        self.pending_captures -= count;
        count
    }
}

#[derive(Debug, Default)]
struct RetryBackoff {
    next_index: usize,
}

impl RetryBackoff {
    fn next_delay(&mut self) -> Duration {
        let index = self.next_index.min(RETRY_DELAYS.len() - 1);
        if self.next_index < RETRY_DELAYS.len() {
            self.next_index += 1;
        }
        RETRY_DELAYS[index]
    }

    fn reset(&mut self) {
        self.next_index = 0;
    }
}

fn run_camera(
    options: WorkerOptions,
    control: &AgentControl,
    monitor: &AgentMonitor,
    preview: &PreviewHub,
) -> Result<(), String> {
    // Starting an attempt clears any image from the previous camera session,
    // even if SDK loading or enumeration fails before capture begins.
    let preview_session = preview.begin_session();
    let config = autopiercam_core::config::Config::load(&options.config_path)
        .map_err(|e| format!("loading camera configuration: {e}"))?;
    let sdk = Driver::new(
        options.sdk_path.as_deref(),
        config.camera.driver,
        config.camera.serial,
    )
    .map(Arc::new)
    .map_err(|error| {
        let message = format!("loading Regain camera driver: {error}");
        monitor.report_camera_inventory(Err(message.clone()));
        message
    })?;

    run_agent_with_monitor_and_preview(
        &sdk,
        &options.config_path,
        None,
        control,
        monitor,
        &preview_session,
    )
    .map_err(|error| format!("capture worker failed: {error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fault_requires_an_operator_restart_or_configuration_save() {
        let mut intent = SupervisorIntent::new(false);
        intent.faulted = true;
        for command in [TrayCommand::CaptureNow, TrayCommand::SetPaused(false)] {
            assert_eq!(intent.accept(command, false), LifecycleRequest::None);
            assert!(intent.faulted);
        }
        assert_eq!(
            intent.accept(TrayCommand::Restart, false),
            LifecycleRequest::Restart
        );
        assert!(!intent.faulted);
        intent.faulted = true;
        assert_eq!(
            intent.accept(TrayCommand::ReloadConfiguration, false),
            LifecycleRequest::Restart
        );
        assert!(!intent.faulted);
    }

    #[test]
    fn quit_watchdog_uses_injected_process_exit_and_stops_without_owner() {
        let signals = Arc::new(WorkerSignals::default());
        signals.stopping.store(true, Ordering::Release);
        let timed_out = AtomicBool::new(false);
        shutdown_watchdog(Arc::downgrade(&signals), Duration::ZERO, || {
            timed_out.store(true, Ordering::Release);
        });
        assert!(timed_out.load(Ordering::Acquire));
        shutdown_watchdog(Weak::new(), Duration::ZERO, || panic!("owner already gone"));
    }

    #[test]
    fn quit_deadline_terminates_only_the_synthetic_child_process() {
        const CHILD_FLAG: &str = "AUTOPIERCAM_TEST_QUIT_WATCHDOG_CHILD";
        if std::env::var_os(CHILD_FLAG).is_some() {
            // No SDK, files, uploads, or production process are involved.
            let signals = Arc::new(WorkerSignals::default());
            signals.stopping.store(true, Ordering::Release);
            shutdown_watchdog(
                Arc::downgrade(&signals),
                Duration::from_millis(20),
                terminate_stuck_host,
            );
            panic!("termination returned");
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worker::tests::quit_deadline_terminates_only_the_synthetic_child_process",
            ])
            .env(CHILD_FLAG, "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert_eq!(status.code(), Some(1));
                break;
            }
            if started.elapsed() > Duration::from_secs(10) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("shutdown watchdog failed to terminate its test child");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn restart_retains_stuck_owner_without_blocking_supervisor_or_opening_replacement() {
        let (release, blocked) = mpsc::channel();
        let control = AgentControl::new();
        let mut session = Some(CameraSession {
            control: control.clone(),
            thread: Some(thread::spawn(move || {
                let _ = blocked.recv_timeout(Duration::from_secs(5));
                Ok(())
            })),
            ready: true,
            reached_capturing: true,
            started_capturing_generation: 0,
            stop_requested_at: None,
        });
        let monitor = AgentMonitor::new();
        let mut last_status = None;
        let restart_pending = AtomicBool::new(true);
        let mut intent = SupervisorIntent::new(false);
        assert!(!handle_command(
            TrayCommand::Restart,
            &mut session,
            &mut intent,
            &mut RetryBackoff::default(),
            &mut Instant::now(),
            &restart_pending,
            &monitor,
            &|_| {},
            &mut last_status,
        ));
        assert!(restart_pending.load(Ordering::Acquire));
        assert!(control.is_shutdown());
        assert!(session.is_some());
        assert!(!session.as_ref().unwrap().is_finished());
        assert!(!session.as_ref().unwrap().ready);
        session.as_mut().unwrap().stop_requested_at = Some(Instant::now() - SHUTDOWN_GRACE);
        intent.accept(TrayCommand::CaptureNow, false);
        observe_session_status(
            &monitor,
            &|_| {},
            &mut last_status,
            &mut session,
            &mut intent,
            &mut RetryBackoff::default(),
        );
        assert_eq!(monitor.snapshot().state, AgentState::Faulted);
        assert_eq!(intent.pending_captures, 1);
        release.send(()).unwrap();
        assert_eq!(
            session.take().unwrap().shutdown_and_join(),
            SessionExit::Completed
        );
    }

    #[test]
    fn settings_reload_preserves_pause_and_pending_captures_without_lifecycle_change() {
        let mut intent = SupervisorIntent::new(true);
        intent.accept(TrayCommand::CaptureNow, false);
        for ready in [true, false] {
            assert_eq!(
                intent.accept(TrayCommand::ReloadConfiguration, ready),
                LifecycleRequest::None
            );
            assert!(intent.paused);
        }
        assert_eq!(intent.take_pending_captures(10), 1);
    }

    #[test]
    fn retry_backoff_is_bounded_and_resets_after_capture() {
        let mut backoff = RetryBackoff::default();
        let delays = (0..7).map(|_| backoff.next_delay()).collect::<Vec<_>>();
        assert_eq!(delays, [1, 2, 5, 10, 30, 30, 30].map(Duration::from_secs));

        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn supervisor_intent_preserves_commands_across_reconnect() {
        let mut intent = SupervisorIntent::new(false);

        assert_eq!(
            intent.accept(TrayCommand::SetPaused(true), false),
            LifecycleRequest::None
        );
        assert!(intent.paused);
        assert_eq!(
            intent.accept(TrayCommand::CaptureNow, false),
            LifecycleRequest::None
        );
        assert_eq!(
            intent.accept(TrayCommand::CaptureNow, false),
            LifecycleRequest::None
        );
        assert_eq!(intent.take_pending_captures(1), 1);
        assert_eq!(intent.take_pending_captures(10), 1);
        assert_eq!(intent.take_pending_captures(10), 0);
        assert_eq!(
            intent.accept(TrayCommand::Restart, false),
            LifecycleRequest::Restart
        );
        assert!(intent.paused);
        assert_eq!(
            intent.accept(TrayCommand::Shutdown, false),
            LifecycleRequest::Shutdown
        );
    }

    #[test]
    fn accepting_shutdown_rejects_every_later_command() {
        let (sender, receiver) = mpsc::channel();
        let directory = tempfile::tempdir().unwrap();
        let sharing = autopiercam::SharingService::start(
            &directory.path().join("config.toml"),
            Arc::new(|| None),
        )
        .unwrap();
        let client = WorkerClient {
            sharing: Some(sharing.client()),
            commands: Arc::new(Mutex::new(sender)),
            monitor: AgentMonitor::new(),
            preview: PreviewHub::new(),
            signals: Arc::new(WorkerSignals::default()),
            thread: Arc::new(Mutex::new(None)),
        };

        client.send(TrayCommand::Shutdown).unwrap();
        assert!(client.send(TrayCommand::Restart).is_err());
        assert_eq!(receiver.try_recv().unwrap(), TrayCommand::Shutdown);
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn repeated_restart_requests_are_coalesced_until_session_stops() {
        let (sender, receiver) = mpsc::channel();
        let directory = tempfile::tempdir().unwrap();
        let sharing = autopiercam::SharingService::start(
            &directory.path().join("config.toml"),
            Arc::new(|| None),
        )
        .unwrap();
        let signals = Arc::new(WorkerSignals::default());
        let client = WorkerClient {
            sharing: Some(sharing.client()),
            commands: Arc::new(Mutex::new(sender)),
            monitor: AgentMonitor::new(),
            preview: PreviewHub::new(),
            signals: Arc::clone(&signals),
            thread: Arc::new(Mutex::new(None)),
        };

        client.send(TrayCommand::Restart).unwrap();
        client.send(TrayCommand::Restart).unwrap();
        assert_eq!(receiver.try_recv().unwrap(), TrayCommand::Restart);
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));

        signals.restart_pending.store(false, Ordering::Release);
        client.send(TrayCommand::Restart).unwrap();
        assert_eq!(receiver.try_recv().unwrap(), TrayCommand::Restart);
    }
}
