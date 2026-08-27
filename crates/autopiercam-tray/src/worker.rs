use std::{
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::mpsc::{self, Receiver, SendError, Sender},
    thread,
};

/// Commands emitted by the tray UI for the capture worker or its future IPC adapter.
#[derive(Debug)]
pub(crate) enum TrayCommand {
    SetPaused(bool),
    CaptureNow,
    Shutdown,
}

/// Worker-owned state suitable for showing in the tray and configuration UI.
#[derive(Clone, Debug)]
pub(crate) struct WorkerStatus {
    pub(crate) paused: bool,
    pub(crate) summary: String,
}

/// Events emitted by either the in-process placeholder or a future IPC transport.
#[derive(Debug)]
pub(crate) enum WorkerEvent {
    StatusChanged(WorkerStatus),
    WorkerStopped,
}

/// UI-facing command transport. Its implementation can later become a named-pipe client
/// without changing tray menu or event-loop code.
#[derive(Clone)]
pub(crate) struct WorkerClient {
    commands: Sender<TrayCommand>,
}

impl WorkerClient {
    pub(crate) fn send(&self, command: TrayCommand) -> Result<(), SendError<TrayCommand>> {
        self.commands.send(command)
    }
}

/// Starts the checkpoint's local worker stand-in. Replace only this constructor when the
/// background capture process exposes its IPC endpoint.
pub(crate) fn start_placeholder_worker<F>(emit: F) -> io::Result<WorkerClient>
where
    F: Fn(WorkerEvent) + Send + 'static,
{
    let (commands, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("autopiercam-placeholder-worker".to_owned())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                placeholder_worker(receiver, &emit);
            }));
            if result.is_err() {
                emit(WorkerEvent::StatusChanged(WorkerStatus {
                    paused: false,
                    summary: "Capture worker failed".to_owned(),
                }));
            }
            // Tao exits the process rather than returning from EventLoop::run. Always let the
            // tray observe worker cleanup before it selects ControlFlow::Exit.
            emit(WorkerEvent::WorkerStopped);
        })?;
    Ok(WorkerClient { commands })
}

fn placeholder_worker<F>(commands: Receiver<TrayCommand>, emit: &F)
where
    F: Fn(WorkerEvent),
{
    let mut paused = false;
    emit_status(emit, paused, "Capture running (placeholder)");

    while let Ok(command) = commands.recv() {
        match command {
            TrayCommand::SetPaused(value) => {
                paused = value;
                let summary = if paused {
                    "Capture paused (placeholder)"
                } else {
                    "Capture running (placeholder)"
                };
                emit_status(emit, paused, summary);
            }
            TrayCommand::CaptureNow => {
                emit_status(emit, paused, "Capture-now requested (placeholder)");
            }
            TrayCommand::Shutdown => {
                emit_status(emit, paused, "Stopping capture worker");
                break;
            }
        }
    }

    // The real worker closes the ASI camera and drains bounded artifact queues here.
}

fn emit_status<F>(emit: &F, paused: bool, summary: &str)
where
    F: Fn(WorkerEvent),
{
    emit(WorkerEvent::StatusChanged(WorkerStatus {
        paused,
        summary: summary.to_owned(),
    }));
}
