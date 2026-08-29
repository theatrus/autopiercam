use std::{
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use autopiercam_protocol::{MAX_FRAME_SIZE, Method, PIPE_NAME, Request, Response};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, NamedPipeClient},
    runtime::Builder,
    time::{sleep, timeout},
};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Pipes::GetNamedPipeServerProcessId,
        Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    },
};

const CONTROL_PIPE_PATH: &str = r"\\.\pipe\autopiercam-control-v1";
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(50);

pub(crate) fn shutdown_agent(wait: Duration, if_running: bool) -> Result<bool> {
    if wait.is_zero() {
        bail!("shutdown timeout must be greater than zero");
    }
    let deadline = Instant::now()
        .checked_add(wait)
        .ok_or_else(|| anyhow!("shutdown timeout is too large"))?;
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("creating the AutoPierCam control runtime")?;

    let Some((server_process, response)) =
        runtime.block_on(request_shutdown(deadline, if_running))?
    else {
        return Ok(false);
    };
    validate_shutdown_response(&response)?;
    wait_for_process_exit(&server_process, deadline)?;
    Ok(true)
}

async fn request_shutdown(
    deadline: Instant,
    if_running: bool,
) -> Result<Option<(OwnedHandle, Response)>> {
    debug_assert_eq!(PIPE_NAME, "autopiercam-control-v1");
    let Some(mut pipe) = connect(deadline, if_running).await? else {
        return Ok(None);
    };
    let server_process = server_process_handle(&pipe)?;
    let request_id = request_id();
    let request = Request::new(&request_id, Method::AgentShutdown);
    request
        .validate()
        .context("validating the shutdown request")?;
    let encoded = serde_json::to_vec(&request).context("encoding the shutdown request")?;
    if encoded.len() > MAX_FRAME_SIZE {
        bail!("shutdown request exceeds the control protocol limit");
    }
    let request_length = u32::try_from(encoded.len()).expect("the protocol limit fits in u32");

    let response = timeout(remaining(deadline)?, async {
        pipe.write_all(&request_length.to_le_bytes()).await?;
        pipe.write_all(&encoded).await?;
        pipe.flush().await?;

        let mut prefix = [0_u8; 4];
        pipe.read_exact(&mut prefix).await?;
        let response_length = u32::from_le_bytes(prefix) as usize;
        if response_length == 0 || response_length > MAX_FRAME_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "control response length {response_length} is outside 1..={MAX_FRAME_SIZE}"
                ),
            ));
        }
        let mut response = vec![0_u8; response_length];
        pipe.read_exact(&mut response).await?;
        Ok::<_, std::io::Error>(response)
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for the tray agent's shutdown response"))??;

    let response: Response =
        serde_json::from_slice(&response).context("decoding the tray agent's shutdown response")?;
    response
        .validate()
        .context("validating the tray agent's shutdown response")?;
    if response.request_id != request_id {
        bail!(
            "tray agent returned response id {:?}; expected {:?}",
            response.request_id,
            request_id
        );
    }
    Ok(Some((server_process, response)))
}

async fn connect(deadline: Instant, if_running: bool) -> Result<Option<NamedPipeClient>> {
    loop {
        match ClientOptions::new().open(CONTROL_PIPE_PATH) {
            Ok(pipe) => return Ok(Some(pipe)),
            Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
                if if_running {
                    return Ok(None);
                }
                bail!(
                    "the current user's AutoPierCam tray agent is not running ({CONTROL_PIPE_PATH})"
                );
            }
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                let delay = remaining(deadline)?.min(CONNECT_RETRY_DELAY);
                sleep(delay).await;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("connecting to the current user's control pipe {CONTROL_PIPE_PATH}")
                });
            }
        }
    }
}

fn server_process_handle(pipe: &NamedPipeClient) -> Result<OwnedHandle> {
    let pipe_handle = pipe.as_raw_handle() as HANDLE;
    let mut process_id = 0_u32;
    // SAFETY: pipe_handle is owned by a live NamedPipeClient and process_id is
    // a valid writable u32 for the duration of this call.
    if unsafe { GetNamedPipeServerProcessId(pipe_handle, &mut process_id) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("identifying the AutoPierCam tray process");
    }
    // SAFETY: process_id came from the connected server endpoint. The returned
    // handle, when non-null, is immediately wrapped for deterministic closing.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("opening AutoPierCam tray process {process_id} for waiting"));
    }
    // SAFETY: OpenProcess returned a new owned, non-null HANDLE.
    Ok(unsafe { OwnedHandle::from_raw_handle(process) })
}

fn wait_for_process_exit(process: &OwnedHandle, deadline: Instant) -> Result<()> {
    let remaining = remaining(deadline)?;
    let timeout_ms = u32::try_from(remaining.as_millis().max(1)).unwrap_or(u32::MAX - 1);
    // SAFETY: process is a live synchronization handle and remains owned for
    // the complete wait.
    match unsafe { WaitForSingleObject(process.as_raw_handle() as HANDLE, timeout_ms) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => bail!("timed out waiting for the AutoPierCam tray process to exit"),
        _ => Err(std::io::Error::last_os_error())
            .context("waiting for the AutoPierCam tray process to exit"),
    }
}

fn validate_shutdown_response(response: &Response) -> Result<()> {
    if let Some(error) = &response.error {
        bail!(
            "tray agent rejected shutdown ({}): {}",
            error.code,
            error.message
        );
    }
    let accepted = response
        .result
        .as_ref()
        .and_then(|result| result.get("accepted"))
        .and_then(serde_json::Value::as_bool);
    if accepted != Some(true) {
        bail!("tray agent returned a shutdown response without accepted=true");
    }
    Ok(())
}

fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| anyhow!("timed out shutting down the AutoPierCam tray agent"))
}

fn request_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("shutdown-{}-{timestamp}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use autopiercam_protocol::ErrorBody;

    #[test]
    fn accepted_shutdown_response_is_required() {
        let accepted = Response::success("shutdown", serde_json::json!({ "accepted": true }));
        validate_shutdown_response(&accepted).unwrap();

        let missing = Response::success("shutdown", serde_json::json!({}));
        assert!(validate_shutdown_response(&missing).is_err());

        let rejected = Response::failure(
            "shutdown",
            ErrorBody::new("agent_stopped", "the agent is already stopping"),
        );
        let error = validate_shutdown_response(&rejected).unwrap_err();
        assert!(error.to_string().contains("agent_stopped"));
    }
}
