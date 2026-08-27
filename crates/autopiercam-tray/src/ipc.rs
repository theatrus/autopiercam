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

use autopiercam::{AgentControl, AgentMonitor};
use autopiercam_protocol::{
    ErrorBody, MAX_FRAME_SIZE, Method, PIPE_NAME, Request, Response, ValidationError,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe::{NamedPipeServer, ServerOptions},
    runtime::Builder,
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

const CONTROL_PIPE_PATH: &str = r"\\.\pipe\autopiercam-control-v1";
const IO_POLL_INTERVAL: Duration = Duration::from_millis(100);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct ControlServer {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ControlServer {
    pub(crate) fn start(control: AgentControl, monitor: AgentMonitor) -> io::Result<Self> {
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
                    let server = match create_current_user_pipe(true) {
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
                    if let Err(error) = serve(server, &control, &monitor, &server_stop).await
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
        self.stop.store(true, Ordering::Release);
    }
}

async fn serve(
    mut server: NamedPipeServer,
    control: &AgentControl,
    monitor: &AgentMonitor,
    stop: &AtomicBool,
) -> io::Result<()> {
    loop {
        wait_for_connection(&server, control, stop).await?;
        let mut connected_client = server;
        // Always publish the next instance before servicing this client. A
        // client can connect to it before ConnectNamedPipe is awaited, which
        // avoids a stale/broken connection between one-shot Viewer requests.
        server = create_current_user_pipe(false)?;
        let connection_result =
            serve_connection(&mut connected_client, control, monitor, stop).await;
        drop(connected_client);

        match connection_result {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(()),
            // Client-local failures must not take down the well-known listener.
            // Windows maps disconnects to several codes (109, 232, and 233)
            // depending on whether a read was pending.
            Err(error) => {
                warn!(%error, "local control client disconnected or sent an invalid frame");
            }
        }

        if should_stop(control, stop) {
            return Ok(());
        }
    }
}

async fn wait_for_connection(
    server: &NamedPipeServer,
    control: &AgentControl,
    stop: &AtomicBool,
) -> io::Result<()> {
    let mut connect = Box::pin(server.connect());
    loop {
        if should_stop(control, stop) {
            return Err(stopping_error());
        }
        if let Ok(result) = timeout(IO_POLL_INTERVAL, connect.as_mut()).await {
            return result;
        }
    }
}

async fn serve_connection(
    server: &mut NamedPipeServer,
    control: &AgentControl,
    monitor: &AgentMonitor,
    stop: &AtomicBool,
) -> io::Result<()> {
    loop {
        let Some(request) = read_request(server, control, stop).await? else {
            return Ok(());
        };
        let response = dispatch(request, control, monitor);
        write_response(server, &response).await?;
        if should_stop(control, stop) {
            return Ok(());
        }
    }
}

fn dispatch(request: Request, control: &AgentControl, monitor: &AgentMonitor) -> Response {
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
        Method::CapturePause => {
            control.pause();
            Response::success(request_id, serde_json::json!({ "accepted": true }))
        }
        Method::CaptureResume => {
            control.resume();
            Response::success(request_id, serde_json::json!({ "accepted": true }))
        }
        Method::CaptureNow => {
            control.capture_now();
            Response::success(request_id, serde_json::json!({ "accepted": true }))
        }
        Method::AgentShutdown => {
            monitor.mark_stopping();
            control.shutdown();
            Response::success(request_id, serde_json::json!({ "accepted": true }))
        }
        Method::CamerasList | Method::ConfigGet | Method::ConfigReplace | Method::ArtifactsList => {
            Response::failure(
                request_id,
                ErrorBody::new(
                    "not_implemented",
                    format!(
                        "method {} is not implemented in this checkpoint",
                        request.method
                    ),
                ),
            )
        }
    }
}

fn validation_error(error: ValidationError) -> ErrorBody {
    ErrorBody::new("invalid_request", error.to_string())
}

async fn read_request(
    server: &mut NamedPipeServer,
    control: &AgentControl,
    stop: &AtomicBool,
) -> io::Result<Option<Request>> {
    let mut prefix = [0_u8; 4];
    let first = read_some(server, &mut prefix[..1], control, stop).await?;
    if first == 0 {
        return Ok(None);
    }
    read_exact(server, &mut prefix[1..], control, stop).await?;
    let length = u32::from_le_bytes(prefix) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("control frame is {length} bytes; maximum is {MAX_FRAME_SIZE}"),
        ));
    }

    let mut payload = vec![0_u8; length];
    read_exact(server, &mut payload, control, stop).await?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

async fn read_exact(
    server: &mut NamedPipeServer,
    destination: &mut [u8],
    control: &AgentControl,
    stop: &AtomicBool,
) -> io::Result<()> {
    let mut offset = 0;
    while offset < destination.len() {
        let count = read_some(server, &mut destination[offset..], control, stop).await?;
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
    control: &AgentControl,
    stop: &AtomicBool,
) -> io::Result<usize> {
    // Keep one overlapped ReadFile future alive across stop polls. Recreating
    // it on every timeout cancels the Windows operation and can strand a pipe
    // instance after a client disconnects.
    let mut read = Box::pin(server.read(destination));
    loop {
        if should_stop(control, stop) {
            return Err(stopping_error());
        }
        if let Ok(result) = timeout(IO_POLL_INTERVAL, read.as_mut()).await {
            return result;
        }
    }
}

async fn write_response(server: &mut NamedPipeServer, response: &Response) -> io::Result<()> {
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

fn should_stop(control: &AgentControl, stop: &AtomicBool) -> bool {
    stop.load(Ordering::Acquire) || control.is_shutdown()
}

fn stopping_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "control server is stopping")
}

fn create_current_user_pipe(first_instance: bool) -> io::Result<NamedPipeServer> {
    debug_assert_eq!(PIPE_NAME, "autopiercam-control-v1");
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
        .in_buffer_size(MAX_FRAME_SIZE as u32)
        .out_buffer_size(MAX_FRAME_SIZE as u32);

    // SAFETY: attributes and its LocalAlloc-owned descriptor remain valid for
    // the entire CreateNamedPipeW call. Windows copies the descriptor into the
    // new kernel object before this function returns.
    unsafe {
        options.create_with_security_attributes_raw(
            CONTROL_PIPE_PATH,
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
    use autopiercam_protocol::{AgentState, PROTOCOL_VERSION};

    #[test]
    fn dispatch_reports_status_and_applies_capture_commands() {
        let control = AgentControl::new();
        let monitor = AgentMonitor::new();

        let response = dispatch(
            Request::new("status-1", Method::StatusGet),
            &control,
            &monitor,
        );
        assert_eq!(response.request_id, "status-1");
        assert_eq!(response.version, PROTOCOL_VERSION);
        assert_eq!(
            response.result.expect("status result")["state"],
            serde_json::json!(AgentState::Starting)
        );

        let response = dispatch(
            Request::new("pause-1", Method::CapturePause),
            &control,
            &monitor,
        );
        assert!(response.error.is_none());
        assert!(control.is_paused());

        let response = dispatch(
            Request::new("resume-1", Method::CaptureResume),
            &control,
            &monitor,
        );
        assert!(response.error.is_none());
        assert!(!control.is_paused());
    }

    #[test]
    fn dispatch_returns_structured_error_for_future_methods() {
        let response = dispatch(
            Request::new("config-1", Method::ConfigGet),
            &AgentControl::new(),
            &AgentMonitor::new(),
        );
        assert!(response.result.is_none());
        assert_eq!(response.error.expect("error").code, "not_implemented");
    }
}
