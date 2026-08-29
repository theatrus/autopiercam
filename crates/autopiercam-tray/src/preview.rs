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
    io::{AsyncWrite, AsyncWriteExt},
    net::windows::named_pipe::NamedPipeServer,
    runtime::Builder,
    task::JoinSet,
    time::{sleep, timeout},
};
use tracing::{info, warn};

use crate::ipc::{PipeAccess, create_current_user_pipe};

const PREVIEW_PIPE_PATH: &str = r"\\.\pipe\autopiercam-preview-v1";
const IO_POLL_INTERVAL: Duration = Duration::from_millis(100);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const PIPE_BUFFER_SIZE: u32 = 256 * 1024;
const MAX_ACTIVE_PREVIEW_CLIENTS: usize = 4;

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
                    if let Err(error) = serve(server, preview, server_stop).await
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
    preview: PreviewHub,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    let mut clients = JoinSet::new();
    let accept_result = async {
        loop {
            reap_finished_connections(&mut clients);
            if should_stop(&stop) {
                return Ok(());
            }

            while clients.len() >= MAX_ACTIVE_PREVIEW_CLIENTS {
                wait_for_connection_capacity(&mut clients, &stop).await;
                if should_stop(&stop) {
                    return Ok(());
                }
            }

            if !wait_for_connection(&server, &stop, &mut clients).await? {
                return Ok(());
            }

            let mut connected_client = server;
            // Publish the replacement before dispatching this client. Windows
            // can attach one waiting client before ConnectNamedPipe is polled,
            // so the well-known endpoint stays continuously available even
            // while all four stream tasks are occupied.
            server = create_preview_pipe(false)?;
            let client_preview = preview.clone();
            let client_stop = Arc::clone(&stop);
            clients.spawn(async move {
                serve_connection(&mut connected_client, &client_preview, &client_stop).await
            });
        }
    }
    .await;

    // Dropping an async named-pipe operation cancels its overlapped I/O. Abort
    // every client on listener failure or shutdown, then drain the JoinSet so
    // every connected handle is dropped before the server thread exits.
    abort_and_drain_connections(&mut clients).await;
    accept_result
}

async fn wait_for_connection(
    server: &NamedPipeServer,
    stop: &AtomicBool,
    clients: &mut JoinSet<io::Result<()>>,
) -> io::Result<bool> {
    let mut connect = Box::pin(server.connect());
    loop {
        reap_finished_connections(clients);
        if should_stop(stop) {
            return Ok(false);
        }
        if let Ok(result) = timeout(IO_POLL_INTERVAL, connect.as_mut()).await {
            result?;
            return Ok(true);
        }
    }
}

async fn wait_for_connection_capacity(clients: &mut JoinSet<io::Result<()>>, stop: &AtomicBool) {
    if should_stop(stop) {
        return;
    }
    if let Ok(Some(completion)) = timeout(IO_POLL_INTERVAL, clients.join_next()).await {
        report_connection_completion(completion);
    }
}

fn reap_finished_connections(clients: &mut JoinSet<io::Result<()>>) {
    while let Some(completion) = clients.try_join_next() {
        report_connection_completion(completion);
    }
}

fn report_connection_completion(completion: Result<io::Result<()>, tokio::task::JoinError>) {
    match completion {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
        Ok(Err(error)) => warn!(%error, "preview client disconnected or stopped reading"),
        Err(error) if error.is_cancelled() => {}
        Err(error) => warn!(%error, "preview client task failed"),
    }
}

async fn abort_and_drain_connections(clients: &mut JoinSet<io::Result<()>>) {
    clients.abort_all();
    while let Some(completion) = clients.join_next().await {
        report_connection_completion(completion);
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

async fn write_frame(
    server: &mut (impl AsyncWrite + Unpin),
    frame: &PreviewFrame,
) -> io::Result<()> {
    write_frame_with_timeout(server, frame, WRITE_TIMEOUT).await
}

async fn write_frame_with_timeout(
    server: &mut (impl AsyncWrite + Unpin),
    frame: &PreviewFrame,
    write_timeout: Duration,
) -> io::Result<()> {
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

    timeout(write_timeout, async {
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
        Some(MAX_ACTIVE_PREVIEW_CLIENTS + 1),
    )
}

fn should_stop(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Acquire)
}

fn stopping_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "preview server is stopping")
}

#[cfg(test)]
mod tests {
    use super::*;
    use autopiercam_protocol::{
        PROTOCOL_VERSION, PreviewContentType, PreviewMetadata, PreviewMode,
    };
    use std::{future::pending, sync::atomic::AtomicUsize};

    fn frame(jpeg_payload_size: usize) -> PreviewFrame {
        let mut jpeg = Vec::with_capacity(jpeg_payload_size + 4);
        jpeg.extend_from_slice(&[0xff, 0xd8]);
        jpeg.resize(jpeg_payload_size + 2, 0x42);
        jpeg.extend_from_slice(&[0xff, 0xd9]);
        PreviewFrame {
            metadata: PreviewMetadata {
                version: PROTOCOL_VERSION,
                session_generation: 1,
                sequence: 1,
                captured_at_unix_ms: 1,
                width: 1,
                height: 1,
                exposure_us: Some(1_000),
                gain: Some(10),
                content_type: PreviewContentType::Jpeg,
                mode: PreviewMode::Unknown,
                dropped_frames: 0,
            },
            jpeg: Arc::from(jpeg),
        }
    }

    #[test]
    fn stalled_client_write_does_not_delay_healthy_client_write() {
        let runtime = Builder::new_current_thread().enable_time().build().unwrap();
        runtime.block_on(async {
            let preview = Arc::new(frame(4 * 1024));
            let (mut stalled_writer, stalled_reader) = tokio::io::duplex(16);
            let mut clients = JoinSet::new();

            let stalled_preview = Arc::clone(&preview);
            clients.spawn(async move {
                let _keep_open_without_reading = stalled_reader;
                write_frame_with_timeout(
                    &mut stalled_writer,
                    &stalled_preview,
                    Duration::from_millis(100),
                )
                .await
                .map(|_| "stalled")
            });

            let healthy_preview = Arc::clone(&preview);
            clients.spawn(async move {
                let mut output = Vec::new();
                write_frame_with_timeout(&mut output, &healthy_preview, Duration::from_secs(1))
                    .await?;
                Ok::<_, io::Error>("healthy")
            });

            let first = timeout(Duration::from_millis(50), clients.join_next())
                .await
                .expect("healthy client should finish without waiting for stalled client")
                .expect("a client task should finish")
                .expect("healthy client task should not panic")
                .expect("healthy client write should succeed");
            assert_eq!(first, "healthy");

            let stalled = timeout(Duration::from_secs(1), clients.join_next())
                .await
                .expect("stalled client should respect its write timeout")
                .expect("stalled client task should finish")
                .expect("stalled client task should not panic")
                .expect_err("stalled client write should time out");
            assert_eq!(stalled.kind(), io::ErrorKind::TimedOut);
        });
    }

    #[test]
    fn shutdown_aborts_and_drains_every_client_task() {
        struct DropCounter(Arc<AtomicUsize>);

        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Release);
            }
        }

        let runtime = Builder::new_current_thread().enable_time().build().unwrap();
        runtime.block_on(async {
            let dropped = Arc::new(AtomicUsize::new(0));
            let mut clients = JoinSet::new();
            for _ in 0..MAX_ACTIVE_PREVIEW_CLIENTS {
                let guard = DropCounter(Arc::clone(&dropped));
                clients.spawn(async move {
                    let _guard = guard;
                    pending::<()>().await;
                    Ok(())
                });
            }
            tokio::task::yield_now().await;

            abort_and_drain_connections(&mut clients).await;

            assert!(clients.is_empty());
            assert_eq!(dropped.load(Ordering::Acquire), MAX_ACTIVE_PREVIEW_CLIENTS);
        });
    }
}
