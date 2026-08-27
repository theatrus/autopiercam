use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use autopiercam::{PreviewFrame, PreviewHub};
use autopiercam_protocol::{
    MAX_PREVIEW_JPEG_SIZE, MAX_PREVIEW_METADATA_SIZE, PREVIEW_PIPE_NAME, validate_preview_jpeg,
};
use tokio::{
    io::AsyncWriteExt,
    net::windows::named_pipe::NamedPipeServer,
    runtime::Builder,
    time::{sleep, timeout},
};
use tracing::{info, warn};

use crate::ipc::{PipeAccess, create_current_user_pipe};

const PREVIEW_PIPE_PATH: &str = r"\\.\pipe\autopiercam-preview-v1";
const IO_POLL_INTERVAL: Duration = Duration::from_millis(100);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const PIPE_BUFFER_SIZE: u32 = 256 * 1024;
const PIPE_INSTANCES: usize = 2;

pub(crate) struct PreviewServer {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PreviewServer {
    pub(crate) fn start(preview: PreviewHub) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("autopiercam-preview-pipe".to_owned())
            .spawn(move || {
                let runtime = match Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };

                runtime.block_on(async move {
                    let server = match create_preview_pipe(true) {
                        Ok(server) => server,
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                            return;
                        }
                    };
                    if ready_tx.send(Ok(())).is_err() {
                        return;
                    }
                    info!(pipe = PREVIEW_PIPE_PATH, "local preview pipe is ready");
                    if let Err(error) = serve(server, &preview, &server_stop).await
                        && error.kind() != io::ErrorKind::Interrupted
                    {
                        warn!(%error, "local preview pipe stopped with an error");
                    }
                });
            })?;

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => {
                stop.store(true, Ordering::Release);
                let _ = thread.join();
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("preview-pipe startup handshake failed: {error}"),
                ))
            }
        }
    }

    pub(crate) fn stop_and_join(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_| io::Error::other("preview-pipe thread panicked"))
    }
}

impl Drop for PreviewServer {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

async fn serve(
    mut server: NamedPipeServer,
    preview: &PreviewHub,
    stop: &AtomicBool,
) -> io::Result<()> {
    loop {
        wait_for_connection(&server, stop).await?;
        let mut connected_client = server;
        // Keep an owned listener present while one Viewer is streaming. This
        // avoids a name-ownership gap and lets one replacement client wait.
        server = create_preview_pipe(false)?;
        let connection_result = serve_connection(&mut connected_client, preview, stop).await;
        drop(connected_client);

        match connection_result {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Err(error) => warn!(%error, "preview client disconnected or stopped reading"),
        }
        if should_stop(stop) {
            return Ok(());
        }
    }
}

async fn wait_for_connection(server: &NamedPipeServer, stop: &AtomicBool) -> io::Result<()> {
    let mut connect = Box::pin(server.connect());
    loop {
        if should_stop(stop) {
            return Err(stopping_error());
        }
        if let Ok(result) = timeout(IO_POLL_INTERVAL, connect.as_mut()).await {
            return result;
        }
    }
}

async fn serve_connection(
    server: &mut NamedPipeServer,
    preview: &PreviewHub,
    stop: &AtomicBool,
) -> io::Result<()> {
    let mut observed_change = 0_u64;
    let mut sent_frame = false;

    loop {
        if should_stop(stop) {
            return Err(stopping_error());
        }
        let snapshot = preview.snapshot();
        if snapshot.change_generation != observed_change {
            observed_change = snapshot.change_generation;
            match snapshot.frame {
                Some(frame) => {
                    write_frame(server, &frame).await?;
                    sent_frame = true;
                }
                None if sent_frame => {
                    // A camera restart/fault clears the process-lifetime hub.
                    // EOF tells the Viewer to clear that session immediately.
                    return Ok(());
                }
                None => {}
            }
        }
        sleep(IO_POLL_INTERVAL).await;
    }
}

async fn write_frame(server: &mut NamedPipeServer, frame: &PreviewFrame) -> io::Result<()> {
    frame
        .metadata
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    validate_preview_jpeg(&frame.jpeg)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let metadata = serde_json::to_vec(&frame.metadata)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if metadata.is_empty() || metadata.len() > MAX_PREVIEW_METADATA_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "preview metadata is {} bytes; maximum is {MAX_PREVIEW_METADATA_SIZE}",
                metadata.len()
            ),
        ));
    }
    if frame.jpeg.is_empty() || frame.jpeg.len() > MAX_PREVIEW_JPEG_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "preview JPEG is {} bytes; maximum is {MAX_PREVIEW_JPEG_SIZE}",
                frame.jpeg.len()
            ),
        ));
    }
    let metadata_length =
        u32::try_from(metadata.len()).expect("preview metadata limit fits in u32");
    let jpeg_length = u32::try_from(frame.jpeg.len()).expect("preview JPEG limit fits in u32");

    timeout(WRITE_TIMEOUT, async {
        server.write_all(&metadata_length.to_le_bytes()).await?;
        server.write_all(&metadata).await?;
        server.write_all(&jpeg_length.to_le_bytes()).await?;
        server.write_all(&frame.jpeg).await?;
        server.flush().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "preview frame write timed out"))?
}

fn create_preview_pipe(first_instance: bool) -> io::Result<NamedPipeServer> {
    debug_assert_eq!(PREVIEW_PIPE_NAME, "autopiercam-preview-v1");
    create_current_user_pipe(
        PREVIEW_PIPE_PATH,
        first_instance,
        PipeAccess::Outbound,
        0,
        PIPE_BUFFER_SIZE,
        Some(PIPE_INSTANCES),
    )
}

fn should_stop(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Acquire)
}

fn stopping_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "preview server is stopping")
}
