use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use autopiercam::{AgentControl, AgentMonitor, run_agent_with_monitor};
use autopiercam_asi::Sdk;
use autopiercam_protocol::{AgentState, AgentStatus};

const SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_millis(100);
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
    Shutdown,
}

#[derive(Debug)]
pub(crate) enum WorkerEvent {
    StatusChanged(AgentStatus),
    WorkerStopped,
}

#[derive(Clone)]
pub(crate) struct WorkerClient {
    commands: Arc<Mutex<Sender<TrayCommand>>>,
    monitor: AgentMonitor,
    signals: Arc<WorkerSignals>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
}

#[derive(Debug, Default)]
struct WorkerSignals {
    restart_pending: AtomicBool,
    start_admission: Mutex<()>,
    stopping: AtomicBool,
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
        match command {
            TrayCommand::Restart if self.signals.restart_pending.swap(true, Ordering::AcqRel) => {
                return Ok(());
            }
            TrayCommand::Shutdown => self.signals.stopping.store(true, Ordering::Release),
            TrayCommand::SetPaused(_) | TrayCommand::CaptureNow | TrayCommand::Restart => {}
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
    let signals = Arc::new(WorkerSignals::default());

    let supervisor_monitor = monitor.clone();
    let supervisor_signals = Arc::clone(&signals);
    let thread = thread::Builder::new()
        .name("autopiercam-supervisor".to_owned())
        .spawn(move || {
            let mut last_status = None;
            let result = catch_unwind(AssertUnwindSafe(|| {
                supervise_camera(
                    options,
                    receiver,
                    &supervisor_signals,
                    &supervisor_monitor,
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
        commands: Arc::new(Mutex::new(commands)),
        monitor,
        signals,
        thread: Arc::new(Mutex::new(Some(thread))),
    })
}

fn supervise_camera<F>(
    options: WorkerOptions,
    commands: Receiver<TrayCommand>,
    signals: &WorkerSignals,
    monitor: &AgentMonitor,
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
            let outcome = finished.join_finished();
            report_unexpected_exit(monitor, outcome);
            publish_status_if_changed(monitor, emit, last_status);
            if reached_capturing {
                backoff.reset();
            }
            retry_at = Instant::now() + backoff.next_delay();
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
                orderly_shutdown(&mut session, monitor, emit, last_status);
                return;
            }
            Err(TryRecvError::Empty) => {}
        }

        if session.is_none() && Instant::now() >= retry_at {
            let _admission = signals
                .start_admission
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if signals.stopping.load(Ordering::Acquire) {
                continue;
            }
            match CameraSession::start(&options, monitor, intent.paused) {
                Ok(camera) => session = Some(camera),
                Err(error) => {
                    monitor.report_fault(format!("failed to start camera thread: {error}"));
                    publish_status_if_changed(monitor, emit, last_status);
                    retry_at = Instant::now() + backoff.next_delay();
                }
            }
            continue;
        }

        let wait = session
            .as_ref()
            .map(|_| SUPERVISOR_POLL_INTERVAL)
            .unwrap_or_else(|| {
                retry_at
                    .saturating_duration_since(Instant::now())
                    .min(SUPERVISOR_POLL_INTERVAL)
            });
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
    if command == TrayCommand::Restart {
        restart_pending.store(false, Ordering::Release);
    }
    let status = monitor.snapshot();
    let session_ready = matches!(status.state, AgentState::Capturing | AgentState::Paused)
        && session
            .as_ref()
            .is_some_and(|camera| camera.ready && !camera.is_finished());
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
        TrayCommand::CaptureNow | TrayCommand::Restart | TrayCommand::Shutdown => {}
    }

    match lifecycle {
        LifecycleRequest::None => false,
        LifecycleRequest::Restart => {
            restart_session(session, monitor, emit, last_status);
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
    if let Some(camera) = session.take() {
        report_controlled_exit(monitor, camera.shutdown_and_join());
    }
    monitor.mark_stopping();
    publish_status_if_changed(monitor, emit, last_status);
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
    emit(WorkerEvent::StatusChanged(status.clone()));
    *last_status = Some(status);
}

struct CameraSession {
    control: AgentControl,
    thread: Option<JoinHandle<Result<(), String>>>,
    ready: bool,
    reached_capturing: bool,
    started_capturing_generation: u64,
}

impl CameraSession {
    fn start(
        options: &WorkerOptions,
        monitor: &AgentMonitor,
        paused: bool,
    ) -> std::io::Result<Self> {
        let control = AgentControl::new();
        if paused {
            control.pause();
        }
        let camera_options = options.clone();
        let camera_control = control.clone();
        let camera_monitor = monitor.clone();
        let started_capturing_generation = monitor.capturing_generation();
        let thread = thread::Builder::new()
            .name("autopiercam-camera".to_owned())
            .spawn(move || run_camera(camera_options, &camera_control, &camera_monitor))?;
        Ok(Self {
            control,
            thread: Some(thread),
            ready: false,
            reached_capturing: false,
            started_capturing_generation,
        })
    }

    fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
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
        // A supervisor panic must not detach the camera owner. Agent SDK calls use bounded waits,
        // so requesting shutdown and joining here is finite under the worker contract.
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
    paused: bool,
    pending_captures: u64,
}

impl SupervisorIntent {
    fn new(paused: bool) -> Self {
        Self {
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
            TrayCommand::Restart => LifecycleRequest::Restart,
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let client = WorkerClient {
            commands: Arc::new(Mutex::new(sender)),
            monitor: AgentMonitor::new(),
            signals: Arc::new(WorkerSignals::default()),
            thread: Arc::new(Mutex::new(None)),
        };

        client.send(TrayCommand::Shutdown).unwrap();
        assert!(client.send(TrayCommand::Restart).is_err());
        assert_eq!(receiver.try_recv().unwrap(), TrayCommand::Shutdown);
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn repeated_restart_requests_are_coalesced_until_dequeued() {
        let (sender, receiver) = mpsc::channel();
        let signals = Arc::new(WorkerSignals::default());
        let client = WorkerClient {
            commands: Arc::new(Mutex::new(sender)),
            monitor: AgentMonitor::new(),
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
