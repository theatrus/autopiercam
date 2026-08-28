use std::{
    ffi::c_void,
    io, ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use autopiercam::{AgentMonitor, UploadAdminError};
use autopiercam_core::{
    ConfigStore, ConfigStoreError,
    config::{Config, ConfigError},
};
use autopiercam_protocol::{
    Complete, ConfigReplace, ConfigSaved, ConfigSnapshot as ProtocolConfigSnapshot, ErrorBody,
    MAX_FRAME_SIZE, Method, PIPE_NAME, Request, Response, RevisionConflictDetails,
    UploadListRequest, UploadRequeueRequest, ValidationError,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe::{NamedPipeServer, ServerOptions},
    runtime::Builder,
    task::{self, JoinSet},
    time::timeout,
};
use tracing::{info, warn};
use windows_sys::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::PWSTR,
};

use crate::worker::{TrayCommand, WorkerClient, WorkerStopped};

const CONTROL_PIPE_PATH: &str = r"\\.\pipe\autopiercam-control-v1";
const IO_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_IN_FLIGHT_CONNECTIONS: usize = 8;

#[derive(Clone)]
struct DispatchContext {
    worker: WorkerClient,
    monitor: AgentMonitor,
    config_store: ConfigStore,
}

impl DispatchContext {
    fn dispatch(&self, request: Request) -> Response {
        dispatch(request, &self.worker, &self.monitor, &self.config_store)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectionOutcome {
    Served,
    ShutdownAccepted,
}

pub(crate) struct ControlServer {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ControlServer {
    pub(crate) fn start(
        worker: WorkerClient,
        monitor: AgentMonitor,
        config_store: ConfigStore,
    ) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("autopiercam-ipc".to_owned())
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
                    let server = match create_control_pipe(true) {
                        Ok(server) => server,
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                            return;
                        }
                    };
                    if ready_tx.send(Ok(())).is_err() {
                        return;
                    }
                    info!(pipe = CONTROL_PIPE_PATH, "local control pipe is ready");
                    let context = DispatchContext {
                        worker,
                        monitor,
                        config_store,
                    };
                    if let Err(error) = serve(server, context, server_stop).await
                        && error.kind() != io::ErrorKind::Interrupted
                    {
                        warn!(%error, "local control pipe stopped with an error");
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
                    format!("control-pipe startup handshake failed: {error}"),
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
            .map_err(|_| io::Error::other("control-pipe thread panicked"))
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

async fn serve(
    mut server: NamedPipeServer,
    context: DispatchContext,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    let mut clients = JoinSet::new();
    let accept_result = async {
        loop {
            reap_finished_connections(&mut clients);
            if should_stop(&stop) {
                return Ok(());
            }

            while clients.len() >= MAX_IN_FLIGHT_CONNECTIONS {
                wait_for_connection_capacity(&mut clients, &stop).await;
                if should_stop(&stop) {
                    return Ok(());
                }
            }

            if !wait_for_connection(&server, &stop, &mut clients).await? {
                return Ok(());
            }

            let mut connected_client = server;
            // Always publish the next instance before servicing this client. A
            // client can connect to it before ConnectNamedPipe is awaited, which
            // avoids a stale/broken connection between one-shot Viewer requests.
            server = create_control_pipe(false)?;
            let client_context = context.clone();
            let client_stop = Arc::clone(&stop);
            clients.spawn(async move {
                serve_connection(&mut connected_client, client_context, client_stop).await
            });
        }
    }
    .await;

    // An acceptance failure is terminal, so interrupt clients that are still
    // reading. Requests already in blocking dispatch are allowed to finish.
    if accept_result.is_err() {
        stop.store(true, Ordering::Release);
    }
    while let Some(completion) = clients.join_next().await {
        report_connection_completion(completion);
    }
    accept_result
}

async fn wait_for_connection(
    server: &NamedPipeServer,
    stop: &AtomicBool,
    clients: &mut JoinSet<io::Result<ConnectionOutcome>>,
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

async fn wait_for_connection_capacity(
    clients: &mut JoinSet<io::Result<ConnectionOutcome>>,
    stop: &AtomicBool,
) {
    if should_stop(stop) {
        return;
    }
    if let Ok(Some(completion)) = timeout(IO_POLL_INTERVAL, clients.join_next()).await {
        report_connection_completion(completion);
    }
}

fn reap_finished_connections(clients: &mut JoinSet<io::Result<ConnectionOutcome>>) {
    while let Some(completion) = clients.try_join_next() {
        report_connection_completion(completion);
    }
}

fn report_connection_completion(
    completion: Result<io::Result<ConnectionOutcome>, tokio::task::JoinError>,
) {
    match completion {
        Ok(Ok(ConnectionOutcome::Served | ConnectionOutcome::ShutdownAccepted)) => {}
        // Client-local failures must not take down the well-known listener.
        // Windows maps disconnects to several codes (109, 232, and 233)
        // depending on whether a read was pending.
        Ok(Err(error)) => {
            warn!(%error, "local control client disconnected or sent an invalid frame");
        }
        Err(error) => warn!(%error, "local control client task failed"),
    }
}

async fn serve_connection(
    server: &mut NamedPipeServer,
    context: DispatchContext,
    stop: Arc<AtomicBool>,
) -> io::Result<ConnectionOutcome> {
    // Protocol v1 deliberately serves one request per connection. This keeps
    // an idle client from monopolizing the listener used by status and Quit.
    let Some(request) = timeout(CLIENT_REQUEST_TIMEOUT, read_request(server, &stop))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "control client did not send a request within 5 seconds",
            )
        })??
    else {
        return Ok(ConnectionOutcome::Served);
    };
    let is_shutdown = request.method == Method::AgentShutdown;
    let response = run_blocking_dispatch(request, move |request| context.dispatch(request)).await?;
    let shutdown_accepted = is_shutdown && response.error.is_none();
    send_response_and_finish(server, &response, shutdown_accepted, &stop).await
}

async fn run_blocking_dispatch<F>(request: Request, dispatch: F) -> io::Result<Response>
where
    F: FnOnce(Request) -> Response + Send + 'static,
{
    task::spawn_blocking(move || dispatch(request))
        .await
        .map_err(|error| io::Error::other(format!("control request dispatch failed: {error}")))
}

async fn send_response_and_finish(
    server: &mut (impl tokio::io::AsyncWrite + Unpin),
    response: &Response,
    shutdown_accepted: bool,
    stop: &AtomicBool,
) -> io::Result<ConnectionOutcome> {
    write_response(server, response).await?;
    if shutdown_accepted {
        // Publish shutdown only after the response is flushed so AgentShutdown
        // never tears down its own connection before acknowledging the caller.
        stop.store(true, Ordering::Release);
        Ok(ConnectionOutcome::ShutdownAccepted)
    } else {
        Ok(ConnectionOutcome::Served)
    }
}

trait CommandSink {
    fn send_command(&self, command: TrayCommand) -> Result<(), WorkerStopped>;
}

impl CommandSink for WorkerClient {
    fn send_command(&self, command: TrayCommand) -> Result<(), WorkerStopped> {
        self.send(command)
    }
}

fn dispatch(
    request: Request,
    commands: &impl CommandSink,
    monitor: &AgentMonitor,
    config_store: &ConfigStore,
) -> Response {
    if let Err(error) = request.validate() {
        return Response::failure(request.request_id, validation_error(error));
    }

    let request_id = request.request_id;
    match request.method {
        Method::StatusGet => match serde_json::to_value(monitor.snapshot()) {
            Ok(status) => Response::success(request_id, status),
            Err(error) => Response::failure(
                request_id,
                ErrorBody::new(
                    "internal_error",
                    format!("serializing agent status: {error}"),
                ),
            ),
        },
        Method::CapturePause => command_response(
            request_id,
            commands,
            TrayCommand::SetPaused(true),
            "pause capture",
        ),
        Method::CaptureResume => command_response(
            request_id,
            commands,
            TrayCommand::SetPaused(false),
            "resume capture",
        ),
        Method::CaptureNow => command_response(
            request_id,
            commands,
            TrayCommand::CaptureNow,
            "capture a frame",
        ),
        Method::AgentShutdown => {
            let response = command_response(
                request_id,
                commands,
                TrayCommand::Shutdown,
                "stop the agent",
            );
            if response.error.is_none() {
                monitor.mark_stopping();
            }
            response
        }
        Method::ConfigGet => config_get_response(request_id, config_store),
        Method::ConfigReplace => {
            config_replace_response(request_id, request.payload, commands, config_store)
        }
        Method::UploadsList => upload_list_response(request_id, request.payload, monitor),
        Method::UploadsRequeue => upload_requeue_response(request_id, request.payload, monitor),
        Method::CamerasList | Method::ArtifactsList => Response::failure(
            request_id,
            ErrorBody::new(
                "not_implemented",
                format!(
                    "method {} is not implemented in this checkpoint",
                    request.method
                ),
            ),
        ),
    }
}

fn upload_list_response(
    request_id: String,
    payload: serde_json::Value,
    monitor: &AgentMonitor,
) -> Response {
    let request = match serde_json::from_value::<UploadListRequest>(payload) {
        Ok(request) => request,
        Err(error) => {
            return Response::failure(
                request_id,
                ErrorBody::new(
                    "invalid_upload_request",
                    format!("uploads.list payload is invalid: {error}"),
                ),
            );
        }
    };
    if let Err(error) = request.validate() {
        return Response::failure(
            request_id,
            ErrorBody::new("invalid_upload_request", error.to_string()),
        );
    }
    match monitor.list_uploads(&request) {
        Ok(page) => serialize_success(request_id, page, "upload list"),
        Err(error) => Response::failure(request_id, upload_admin_error(error)),
    }
}

fn upload_requeue_response(
    request_id: String,
    payload: serde_json::Value,
    monitor: &AgentMonitor,
) -> Response {
    let request = match serde_json::from_value::<UploadRequeueRequest>(payload) {
        Ok(request) => request,
        Err(error) => {
            return Response::failure(
                request_id,
                ErrorBody::new(
                    "invalid_upload_request",
                    format!("uploads.requeue payload is invalid: {error}"),
                ),
            );
        }
    };
    if let Err(error) = request.validate() {
        return Response::failure(
            request_id,
            ErrorBody::new("invalid_upload_request", error.to_string()),
        );
    }
    match monitor.requeue_upload(&request) {
        Ok(result) => serialize_success(request_id, result, "upload requeue result"),
        Err(error) => Response::failure(request_id, upload_admin_error(error)),
    }
}

fn upload_admin_error(error: UploadAdminError) -> ErrorBody {
    let code = match error {
        UploadAdminError::ServiceUnavailable => "upload_service_unavailable",
        UploadAdminError::InvalidRequest => "invalid_upload_request",
        UploadAdminError::InvalidCursor => "invalid_upload_cursor",
        UploadAdminError::StaleCursor => "stale_upload_cursor",
        UploadAdminError::CursorParametersMismatch => "upload_cursor_mismatch",
        UploadAdminError::LedgerMismatch => "upload_ledger_conflict",
        UploadAdminError::JobNotFound => "upload_job_not_found",
        UploadAdminError::WrongState => "upload_state_conflict",
        UploadAdminError::StaleJobRevision => "upload_revision_conflict",
        UploadAdminError::DeliveryBindingMismatch => "upload_delivery_conflict",
        UploadAdminError::ArtifactUnavailable => "upload_artifact_unavailable",
        UploadAdminError::ArtifactChanged => "upload_artifact_changed",
        UploadAdminError::Internal => "internal_error",
    };
    ErrorBody::new(code, error.to_string())
}

fn command_response(
    request_id: String,
    commands: &impl CommandSink,
    command: TrayCommand,
    action: &'static str,
) -> Response {
    match commands.send_command(command) {
        Ok(()) => Response::success(request_id, serde_json::json!({ "accepted": true })),
        Err(error) => Response::failure(
            request_id,
            ErrorBody::new("agent_stopped", format!("could not {action}: {error}")),
        ),
    }
}

fn config_get_response(request_id: String, config_store: &ConfigStore) -> Response {
    match config_store.snapshot() {
        Ok(snapshot) => serialize_success(
            request_id,
            ProtocolConfigSnapshot {
                revision: snapshot.revision,
                config: snapshot.config,
            },
            "configuration snapshot",
        ),
        Err(error) => Response::failure(request_id, config_store_error(error)),
    }
}

fn config_replace_response(
    request_id: String,
    payload: serde_json::Value,
    commands: &impl CommandSink,
    config_store: &ConfigStore,
) -> Response {
    let replacement = match serde_json::from_value::<ConfigReplace<Complete<Config>>>(payload) {
        Ok(replacement) => replacement,
        Err(error) => {
            return Response::failure(
                request_id,
                ErrorBody::new(
                    "invalid_config",
                    format!("config.replace payload is invalid: {error}"),
                ),
            );
        }
    };

    match config_store.replace(
        replacement.expected_revision,
        replacement.config.into_inner(),
    ) {
        Ok(snapshot) => match commands.send_command(TrayCommand::Restart) {
            Ok(()) => serialize_success(
                request_id,
                ConfigSaved {
                    revision: snapshot.revision,
                    saved: true,
                    restart_scheduled: true,
                },
                "configuration result",
            ),
            Err(error) => Response::failure(
                request_id,
                ErrorBody::new(
                    "config_saved_agent_stopped",
                    format!(
                        "configuration revision {} was saved, but the agent could not restart: {error}",
                        snapshot.revision
                    ),
                )
                .with_details(serde_json::json!({ "revision": snapshot.revision })),
            ),
        },
        Err(error) => Response::failure(request_id, config_store_error(error)),
    }
}

fn serialize_success(
    request_id: String,
    value: impl serde::Serialize,
    description: &'static str,
) -> Response {
    match serde_json::to_value(value) {
        Ok(value) => Response::success(request_id, value),
        Err(error) => Response::failure(
            request_id,
            ErrorBody::new(
                "internal_error",
                format!("could not serialize {description}: {error}"),
            ),
        ),
    }
}

fn config_store_error(error: ConfigStoreError) -> ErrorBody {
    match error {
        ConfigStoreError::RevisionConflict(conflict) => ErrorBody::new(
            "revision_conflict",
            format!(
                "configuration changed since revision {}; current revision is {}",
                conflict.expected, conflict.current
            ),
        )
        .with_details(
            serde_json::to_value(RevisionConflictDetails {
                expected_revision: conflict.expected,
                current_revision: conflict.current,
            })
            .expect("revision conflict details are always serializable"),
        ),
        ConfigStoreError::Config(ConfigError::Validation(message)) => ErrorBody::new(
            "invalid_config",
            format!("invalid configuration: {message}"),
        ),
        ConfigStoreError::Config(error) => ErrorBody::new("config_unavailable", error.to_string()),
        error => ErrorBody::new("config_store_error", error.to_string()),
    }
}

fn validation_error(error: ValidationError) -> ErrorBody {
    ErrorBody::new("invalid_request", error.to_string())
}

async fn read_request(
    server: &mut NamedPipeServer,
    stop: &AtomicBool,
) -> io::Result<Option<Request>> {
    let mut prefix = [0_u8; 4];
    let first = read_some(server, &mut prefix[..1], stop).await?;
    if first == 0 {
        return Ok(None);
    }
    read_exact(server, &mut prefix[1..], stop).await?;
    let length = u32::from_le_bytes(prefix) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("control frame is {length} bytes; maximum is {MAX_FRAME_SIZE}"),
        ));
    }

    let mut payload = vec![0_u8; length];
    read_exact(server, &mut payload, stop).await?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

async fn read_exact(
    server: &mut NamedPipeServer,
    destination: &mut [u8],
    stop: &AtomicBool,
) -> io::Result<()> {
    let mut offset = 0;
    while offset < destination.len() {
        let count = read_some(server, &mut destination[offset..], stop).await?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "control frame ended before its declared length",
            ));
        }
        offset += count;
    }
    Ok(())
}

async fn read_some(
    server: &mut NamedPipeServer,
    destination: &mut [u8],
    stop: &AtomicBool,
) -> io::Result<usize> {
    // Keep one overlapped ReadFile future alive across stop polls. Recreating
    // it on every timeout cancels the Windows operation and can strand a pipe
    // instance after a client disconnects.
    let mut read = Box::pin(server.read(destination));
    loop {
        if should_stop(stop) {
            return Err(stopping_error());
        }
        if let Ok(result) = timeout(IO_POLL_INTERVAL, read.as_mut()).await {
            return result;
        }
    }
}

async fn write_response(
    server: &mut (impl tokio::io::AsyncWrite + Unpin),
    response: &Response,
) -> io::Result<()> {
    response
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let payload = serde_json::to_vec(response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control response exceeds the protocol frame limit",
        ));
    }
    let length = u32::try_from(payload.len()).expect("1 MiB fits in u32");
    timeout(WRITE_TIMEOUT, async {
        server.write_all(&length.to_le_bytes()).await?;
        server.write_all(&payload).await?;
        server.flush().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "control response write timed out"))?
}

fn should_stop(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Acquire)
}

fn stopping_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "control server is stopping")
}

fn create_control_pipe(first_instance: bool) -> io::Result<NamedPipeServer> {
    debug_assert_eq!(PIPE_NAME, "autopiercam-control-v1");
    create_current_user_pipe(
        CONTROL_PIPE_PATH,
        first_instance,
        PipeAccess::Duplex,
        MAX_FRAME_SIZE as u32,
        MAX_FRAME_SIZE as u32,
        Some(MAX_IN_FLIGHT_CONNECTIONS + 1),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PipeAccess {
    Duplex,
    Outbound,
}

pub(crate) fn create_current_user_pipe(
    path: &str,
    first_instance: bool,
    access: PipeAccess,
    in_buffer_size: u32,
    out_buffer_size: u32,
    max_instances: Option<usize>,
) -> io::Result<NamedPipeServer> {
    let descriptor = current_user_security_descriptor()?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())
            .expect("SECURITY_ATTRIBUTES size fits in u32"),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first_instance)
        .reject_remote_clients(true)
        .in_buffer_size(in_buffer_size)
        .out_buffer_size(out_buffer_size);
    if access == PipeAccess::Outbound {
        options.access_inbound(false);
    }
    if let Some(max_instances) = max_instances {
        options.max_instances(max_instances);
    }

    // SAFETY: attributes and its LocalAlloc-owned descriptor remain valid for
    // the entire CreateNamedPipeW call. Windows copies the descriptor into the
    // new kernel object before this function returns.
    unsafe {
        options.create_with_security_attributes_raw(
            path,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
        )
    }
}

struct OwnedSecurityDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for OwnedSecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: ConvertStringSecurityDescriptor... allocated this value
            // with LocalAlloc and ownership has not been transferred.
            unsafe {
                LocalFree(self.0.cast());
            }
        }
    }
}

fn current_user_security_descriptor() -> io::Result<OwnedSecurityDescriptor> {
    let sid = current_user_sid_string()?;
    let sddl = format!("D:P(A;;GA;;;{sid})");
    let wide = sddl.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let mut descriptor = ptr::null_mut();
    // SAFETY: wide is null terminated; descriptor receives a LocalAlloc-owned
    // security descriptor on success.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedSecurityDescriptor(descriptor))
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: OpenProcessToken returned this owned handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn current_user_sid_string() -> io::Result<String> {
    let mut raw_token = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle and raw_token is
    // a writable out parameter.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(raw_token);

    let mut required = 0_u32;
    // SAFETY: This first call intentionally provides no buffer to obtain size.
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(io::Error::last_os_error());
    }
    let word_size = std::mem::size_of::<usize>();
    let word_count = (required as usize).div_ceil(word_size);
    // TOKEN_USER contains pointer-aligned fields, so use a word-aligned backing
    // allocation rather than relying on Vec<u8>'s minimum alignment.
    let mut buffer = vec![0_usize; word_count];
    // SAFETY: buffer is writable for required bytes; TokenUser defines its
    // leading layout as TOKEN_USER.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful TokenUser query initialized TOKEN_USER in buffer.
    let token_user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
    let mut sid_text: PWSTR = ptr::null_mut();
    // SAFETY: token_user.User.Sid remains valid while buffer is alive and
    // sid_text is a writable LocalAlloc-owned output pointer.
    if unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid_text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = wide_pointer_to_string(sid_text);
    // SAFETY: ConvertSidToStringSidW allocated sid_text with LocalAlloc.
    unsafe {
        LocalFree(sid_text.cast());
    }
    result
}

fn wide_pointer_to_string(pointer: PWSTR) -> io::Result<String> {
    if pointer.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a null SID string",
        ));
    }
    let mut length = 0;
    // SAFETY: ConvertSidToStringSidW guarantees a null-terminated allocation.
    while unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: length was determined within the null-terminated allocation.
    let units = unsafe { std::slice::from_raw_parts(pointer, length) };
    String::from_utf16(units).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use autopiercam_protocol::{
        AgentState, CAPABILITY_STORAGE_RETENTION, CAPABILITY_UPLOADS_LIST,
        CAPABILITY_UPLOADS_REQUEUE, PROTOCOL_VERSION,
    };
    use std::{
        fs,
        path::PathBuf,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicU64},
        },
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct TestCommands {
        sent: Mutex<Vec<TrayCommand>>,
        stopped: AtomicBool,
    }

    impl TestCommands {
        fn snapshot(&self) -> Vec<TrayCommand> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl CommandSink for TestCommands {
        fn send_command(&self, command: TrayCommand) -> Result<(), WorkerStopped> {
            if self.stopped.load(Ordering::Acquire) {
                return Err(WorkerStopped);
            }
            self.sent.lock().unwrap().push(command);
            Ok(())
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            loop {
                let id = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("autopiercam-ipc-tests-{}-{id}", std::process::id()));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("could not create test directory {path:?}: {error}"),
                }
            }
        }

        fn store(&self) -> ConfigStore {
            ConfigStore::open(self.0.join("autopiercam.toml")).unwrap()
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn slow_blocking_dispatch_does_not_stall_another_client_task() {
        let runtime = Builder::new_current_thread().enable_time().build().unwrap();
        runtime.block_on(async {
            let slow_started = Arc::new(AtomicBool::new(false));
            let release_slow = Arc::new(AtomicBool::new(false));
            let mut clients = JoinSet::new();

            let slow_started_in_task = Arc::clone(&slow_started);
            let release_slow_in_task = Arc::clone(&release_slow);
            clients.spawn(run_blocking_dispatch(
                Request::new("slow", Method::UploadsRequeue),
                move |request| {
                    slow_started_in_task.store(true, Ordering::Release);
                    while !release_slow_in_task.load(Ordering::Acquire) {
                        std::thread::park_timeout(Duration::from_millis(1));
                    }
                    Response::success(request.request_id, serde_json::json!({ "done": true }))
                },
            ));

            let started = timeout(Duration::from_secs(1), async {
                while !slow_started.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await;
            if started.is_err() {
                release_slow.store(true, Ordering::Release);
                let _ = clients.join_next().await;
                panic!("slow dispatch did not start");
            }

            clients.spawn(run_blocking_dispatch(
                Request::new("fast", Method::StatusGet),
                |request| {
                    Response::success(request.request_id, serde_json::json!({ "ready": true }))
                },
            ));
            let fast_completion = timeout(Duration::from_secs(1), clients.join_next()).await;

            // Release the slow task before asserting so a failure cannot leave
            // a blocking-pool thread stranded during runtime shutdown.
            release_slow.store(true, Ordering::Release);
            let slow_completion = timeout(Duration::from_secs(1), clients.join_next()).await;

            let fast_response = fast_completion
                .expect("fast dispatch should stay responsive")
                .expect("fast task should be present")
                .expect("fast task should not panic")
                .expect("fast dispatch should succeed");
            assert_eq!(fast_response.request_id, "fast");

            let slow_response = slow_completion
                .expect("slow dispatch should finish after release")
                .expect("slow task should be present")
                .expect("slow task should not panic")
                .expect("slow dispatch should succeed");
            assert_eq!(slow_response.request_id, "slow");
        });
    }

    #[test]
    fn shutdown_stops_acceptance_only_after_its_response_is_written() {
        let runtime = Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let response = Response::success("shutdown-1", serde_json::json!({ "accepted": true }));
            let stop = AtomicBool::new(false);
            let mut frame = Vec::new();

            let outcome = send_response_and_finish(&mut frame, &response, true, &stop)
                .await
                .expect("shutdown response should be written");

            assert_eq!(outcome, ConnectionOutcome::ShutdownAccepted);
            assert!(stop.load(Ordering::Acquire));
            let frame_length = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
            assert_eq!(frame_length, frame.len() - 4);
            let decoded: Response = serde_json::from_slice(&frame[4..]).unwrap();
            assert_eq!(decoded.request_id, "shutdown-1");
            assert!(decoded.error.is_none());

            let failed_stop = AtomicBool::new(false);
            let (mut broken_writer, reader) = tokio::io::duplex(64);
            drop(reader);
            assert!(
                send_response_and_finish(&mut broken_writer, &response, true, &failed_stop)
                    .await
                    .is_err()
            );
            assert!(!failed_stop.load(Ordering::Acquire));
        });
    }

    #[test]
    fn dispatch_reports_status_and_queues_capture_commands() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let commands = TestCommands::default();
        let monitor = AgentMonitor::new();

        let response = dispatch(
            Request::new("status-1", Method::StatusGet),
            &commands,
            &monitor,
            &store,
        );
        assert_eq!(response.request_id, "status-1");
        assert_eq!(response.version, PROTOCOL_VERSION);
        assert_eq!(
            response.result.expect("status result")["state"],
            serde_json::json!(AgentState::Starting)
        );

        let status = monitor.snapshot();
        assert_eq!(
            status.capabilities,
            [
                CAPABILITY_UPLOADS_LIST,
                CAPABILITY_UPLOADS_REQUEUE,
                CAPABILITY_STORAGE_RETENTION
            ]
        );

        let response = dispatch(
            Request::new("pause-1", Method::CapturePause),
            &commands,
            &monitor,
            &store,
        );
        assert!(response.error.is_none());

        let response = dispatch(
            Request::new("resume-1", Method::CaptureResume),
            &commands,
            &monitor,
            &store,
        );
        assert!(response.error.is_none());

        let response = dispatch(
            Request::new("capture-1", Method::CaptureNow),
            &commands,
            &monitor,
            &store,
        );
        assert!(response.error.is_none());

        assert_eq!(
            commands.snapshot(),
            [
                TrayCommand::SetPaused(true),
                TrayCommand::SetPaused(false),
                TrayCommand::CaptureNow,
            ]
        );
    }

    #[test]
    fn configuration_roundtrip_restarts_and_rejects_stale_revisions() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let commands = TestCommands::default();
        let monitor = AgentMonitor::new();

        let response = dispatch(
            Request::new("config-get-1", Method::ConfigGet),
            &commands,
            &monitor,
            &store,
        );
        let initial: ProtocolConfigSnapshot<Config> =
            serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(initial.config.capture.interval_ms, 10_000);

        let mut replacement = initial.config.clone();
        replacement.capture.interval_ms = 2_500;
        let request = Request::new("config-replace-1", Method::ConfigReplace).with_payload(
            serde_json::to_value(ConfigReplace {
                expected_revision: initial.revision,
                config: replacement.clone(),
            })
            .unwrap(),
        );
        let response = dispatch(request, &commands, &monitor, &store);
        let saved: ConfigSaved = serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(saved.saved);
        assert!(saved.restart_scheduled);
        assert_ne!(saved.revision, initial.revision);
        assert_eq!(store.snapshot().unwrap().config.capture.interval_ms, 2_500);
        assert_eq!(commands.snapshot(), [TrayCommand::Restart]);

        replacement.capture.interval_ms = 5_000;
        let stale = Request::new("config-replace-stale", Method::ConfigReplace).with_payload(
            serde_json::to_value(ConfigReplace {
                expected_revision: initial.revision,
                config: replacement,
            })
            .unwrap(),
        );
        let response = dispatch(stale, &commands, &monitor, &store);
        let error = response.error.expect("stale replacement error");
        assert_eq!(error.code, "revision_conflict");
        assert_eq!(
            error.details.unwrap()["current_revision"],
            serde_json::json!(saved.revision)
        );
        assert_eq!(store.snapshot().unwrap().revision, saved.revision);
        assert_eq!(commands.snapshot(), [TrayCommand::Restart]);

        let stopped_commands = TestCommands::default();
        stopped_commands.stopped.store(true, Ordering::Release);
        let current = store.snapshot().unwrap();
        let mut persisted_without_restart = current.config;
        persisted_without_restart.capture.interval_ms = 7_500;
        let request = Request::new("config-replace-stopped", Method::ConfigReplace).with_payload(
            serde_json::to_value(ConfigReplace {
                expected_revision: current.revision,
                config: persisted_without_restart,
            })
            .unwrap(),
        );
        let response = dispatch(request, &stopped_commands, &monitor, &store);
        let error = response.error.expect("saved but not restarted error");
        assert_eq!(error.code, "config_saved_agent_stopped");
        let persisted_revision = error.details.unwrap()["revision"].as_u64().unwrap();
        let persisted = store.snapshot().unwrap();
        assert_eq!(persisted.revision, persisted_revision);
        assert_eq!(persisted.config.capture.interval_ms, 7_500);
    }

    #[test]
    fn invalid_configuration_and_stopped_worker_are_structured_errors() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let commands = TestCommands::default();
        let monitor = AgentMonitor::new();
        let initial = store.snapshot().unwrap();
        let mut invalid = initial.config;
        invalid.capture.interval_ms = 0;

        let request = Request::new("config-invalid", Method::ConfigReplace).with_payload(
            serde_json::to_value(ConfigReplace {
                expected_revision: initial.revision,
                config: invalid,
            })
            .unwrap(),
        );
        let response = dispatch(request, &commands, &monitor, &store);
        assert_eq!(response.error.unwrap().code, "invalid_config");
        assert_eq!(store.snapshot().unwrap().revision, initial.revision);

        let partial =
            Request::new("config-partial", Method::ConfigReplace).with_payload(serde_json::json!({
                "expected_revision": initial.revision,
                "config": { "capture": { "interval_ms": 2_500 } }
            }));
        let response = dispatch(partial, &commands, &monitor, &store);
        let error = response.error.expect("partial replacement error");
        assert_eq!(error.code, "invalid_config");
        assert!(error.message.contains("missing required field config."));
        assert_eq!(store.snapshot().unwrap().revision, initial.revision);

        let current = store.snapshot().unwrap();
        let mut missing_optional = serde_json::to_value(ConfigReplace {
            expected_revision: current.revision,
            config: current.config,
        })
        .unwrap();
        missing_optional["config"]["camera"]
            .as_object_mut()
            .unwrap()
            .remove("camera_id");
        let response = dispatch(
            Request::new("config-missing-optional", Method::ConfigReplace)
                .with_payload(missing_optional),
            &commands,
            &monitor,
            &store,
        );
        assert!(
            response
                .error
                .unwrap()
                .message
                .contains("config.camera.camera_id")
        );

        let current = store.snapshot().unwrap();
        let mut unknown_nested = serde_json::to_value(ConfigReplace {
            expected_revision: current.revision,
            config: current.config,
        })
        .unwrap();
        unknown_nested["config"]["camera"]["unknown"] = serde_json::json!(true);
        let response = dispatch(
            Request::new("config-unknown-nested", Method::ConfigReplace)
                .with_payload(unknown_nested),
            &commands,
            &monitor,
            &store,
        );
        assert_eq!(response.error.unwrap().code, "invalid_config");
        assert_eq!(store.snapshot().unwrap().revision, initial.revision);

        commands.stopped.store(true, Ordering::Release);
        let response = dispatch(
            Request::new("capture-stopped", Method::CaptureNow),
            &commands,
            &monitor,
            &store,
        );
        assert!(response.result.is_none());
        assert_eq!(response.error.expect("error").code, "agent_stopped");
    }

    #[test]
    fn dispatch_returns_structured_error_for_future_methods() {
        let directory = TestDirectory::new();
        let response = dispatch(
            Request::new("cameras-1", Method::CamerasList),
            &TestCommands::default(),
            &AgentMonitor::new(),
            &directory.store(),
        );
        assert!(response.result.is_none());
        assert_eq!(response.error.expect("error").code, "not_implemented");
    }

    #[test]
    fn upload_administration_validates_payloads_and_reports_unavailable_service() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let commands = TestCommands::default();
        let monitor = AgentMonitor::new();

        let unavailable = dispatch(
            Request::new("uploads-list-1", Method::UploadsList),
            &commands,
            &monitor,
            &store,
        );
        assert!(unavailable.result.is_none());
        assert_eq!(
            unavailable.error.expect("unavailable error").code,
            "upload_service_unavailable"
        );

        let invalid_list = dispatch(
            Request::new("uploads-list-invalid", Method::UploadsList)
                .with_payload(serde_json::json!({ "page_size": 0 })),
            &commands,
            &monitor,
            &store,
        );
        assert_eq!(
            invalid_list.error.expect("invalid list error").code,
            "invalid_upload_request"
        );

        let invalid_requeue = dispatch(
            Request::new("uploads-requeue-invalid", Method::UploadsRequeue).with_payload(
                serde_json::json!({
                    "ledger_id": "ledger",
                    "job_id": 0,
                    "expected_job_revision": 1
                }),
            ),
            &commands,
            &monitor,
            &store,
        );
        assert_eq!(
            invalid_requeue.error.expect("invalid requeue error").code,
            "invalid_upload_request"
        );
    }
}
