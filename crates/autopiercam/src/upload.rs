use std::{
    fs::File,
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use tracing::{info, warn};
use ureq::{
    Agent,
    http::{
        HeaderValue, Uri,
        header::{AUTHORIZATION, CONTENT_LENGTH},
    },
    tls::{RootCerts, TlsConfig},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const QUEUE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5 * 60);
const RETRY_DELAYS: [Duration; 7] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
    MAX_RETRY_DELAY,
];
const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

/// Complete configuration needed by one best-effort upload worker.
///
/// The caller validates the endpoint scheme and constructs the authorization
/// value without exposing its source secret to this worker's errors or logs.
pub(crate) struct UploadOptions {
    endpoint: Uri,
    authorization: Option<HeaderValue>,
    queue_capacity: usize,
}

impl UploadOptions {
    pub(crate) fn new(
        endpoint: Uri,
        mut authorization: Option<HeaderValue>,
        queue_capacity: usize,
    ) -> Self {
        if let Some(value) = &mut authorization {
            value.set_sensitive(true);
        }
        Self {
            endpoint,
            authorization,
            queue_capacity,
        }
    }
}

/// Constructs a sensitive `Authorization: Bearer ...` value.
///
/// `InvalidHeaderValue` never includes the rejected bytes, so an invalid token
/// is not surfaced through the returned error.
pub(crate) fn bearer_authorization(
    token: &str,
) -> Result<HeaderValue, ureq::http::header::InvalidHeaderValue> {
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))?;
    value.set_sensitive(true);
    Ok(value)
}

#[derive(Clone)]
pub(crate) struct UploadSink {
    sender: SyncSender<UploadJob>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UploadEnqueueResult {
    Queued,
    QueueFull,
    WorkerStopped,
    InvalidArtifactPath,
}

impl UploadSink {
    /// Attempts to enqueue a finalized artifact without waiting for capacity.
    pub(crate) fn try_enqueue(&self, path: PathBuf) -> UploadEnqueueResult {
        let Ok(job) = UploadJob::new(path) else {
            return UploadEnqueueResult::InvalidArtifactPath;
        };
        match self.sender.try_send(job) {
            Ok(()) => UploadEnqueueResult::Queued,
            Err(TrySendError::Full(_)) => UploadEnqueueResult::QueueFull,
            Err(TrySendError::Disconnected(_)) => UploadEnqueueResult::WorkerStopped,
        }
    }
}

/// Owns the single HTTP thread paired with an [`UploadSink`].
pub(crate) struct UploadWorker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl UploadWorker {
    pub(crate) fn start(options: UploadOptions) -> io::Result<(Self, UploadSink)> {
        if options.queue_capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "upload queue capacity must be greater than zero",
            ));
        }
        let queue_capacity = options.queue_capacity;
        let transport = UreqTransport::new(options);
        Self::start_with_transport(transport, RetryPolicy::production(), queue_capacity)
    }

    fn start_with_transport<T>(
        transport: T,
        retry_policy: RetryPolicy,
        queue_capacity: usize,
    ) -> io::Result<(Self, UploadSink)>
    where
        T: UploadTransport,
    {
        if queue_capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "upload queue capacity must be greater than zero",
            ));
        }
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("autopiercam-upload".to_owned())
            .spawn(move || upload_loop(receiver, &worker_stop, transport, retry_policy))?;
        Ok((
            Self {
                stop,
                thread: Some(thread),
            },
            UploadSink { sender },
        ))
    }

    pub(crate) fn stop_and_join(mut self) -> io::Result<()> {
        self.stop_and_join_inner()
    }

    fn stop_and_join_inner(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        // Wakes an interruptible retry wait immediately. A worker awaiting a
        // new queue item observes cancellation within QUEUE_POLL_INTERVAL.
        thread.thread().unpark();
        thread
            .join()
            .map_err(|_| io::Error::other("upload worker thread panicked"))
    }
}

impl Drop for UploadWorker {
    fn drop(&mut self) {
        let _ = self.stop_and_join_inner();
    }
}

struct UploadJob {
    path: PathBuf,
    file_name: HeaderValue,
    idempotency_key: HeaderValue,
    jitter_seed: u64,
}

impl UploadJob {
    fn new(path: PathBuf) -> Result<Self, ()> {
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or(())?;
        if file_name.is_empty() {
            return Err(());
        }
        let jitter_seed = fnv1a(file_name.as_bytes());
        let file_name_header = HeaderValue::from_str(file_name).map_err(|_| ())?;
        let idempotency_key =
            HeaderValue::from_str(&format!("autopiercam-{file_name}")).map_err(|_| ())?;
        Ok(Self {
            path,
            file_name: file_name_header,
            idempotency_key,
            jitter_seed,
        })
    }
}

trait UploadTransport: Send + 'static {
    fn upload(&mut self, job: &UploadJob) -> AttemptOutcome;
}

struct UreqTransport {
    agent: Agent,
    endpoint: Uri,
    authorization: Option<HeaderValue>,
}

impl UreqTransport {
    fn new(options: UploadOptions) -> Self {
        let tls = TlsConfig::builder()
            .root_certs(RootCerts::PlatformVerifier)
            .build();
        let config = Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(REQUEST_TIMEOUT))
            .max_redirects(0)
            .http_status_as_error(false)
            .tls_config(tls)
            .user_agent(concat!("AutoPierCam/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: config.new_agent(),
            endpoint: options.endpoint,
            authorization: options.authorization,
        }
    }
}

impl UploadTransport for UreqTransport {
    fn upload(&mut self, job: &UploadJob) -> AttemptOutcome {
        let file = match File::open(&job.path) {
            Ok(file) => file,
            Err(_) => return AttemptOutcome::Permanent { status: None },
        };
        let content_length = match file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(_) => return AttemptOutcome::Permanent { status: None },
        };
        let mut request = self
            .agent
            .put(self.endpoint.clone())
            .content_type("image/jpeg")
            .header(CONTENT_LENGTH, content_length.to_string())
            .header("X-AutoPierCam-Filename", job.file_name.clone())
            .header("Idempotency-Key", job.idempotency_key.clone());
        if let Some(authorization) = &self.authorization {
            request = request.header(AUTHORIZATION, authorization.clone());
        }

        match request.send(file) {
            Ok(response) => {
                let status = response.status().as_u16();
                classify_status(status, parse_retry_after(response.headers()))
            }
            // Request construction was prevalidated. Remaining ureq failures
            // are transport/protocol failures and are safe to retry because PUT
            // and the idempotency key remain stable for this artifact.
            Err(_) => AttemptOutcome::Retry {
                status: None,
                retry_after: None,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttemptOutcome {
    Success,
    Retry {
        status: Option<u16>,
        retry_after: Option<Duration>,
    },
    Permanent {
        status: Option<u16>,
    },
}

fn classify_status(status: u16, retry_after: Option<Duration>) -> AttemptOutcome {
    match status {
        200..=299 => AttemptOutcome::Success,
        408 | 425 | 429 | 500..=599 => AttemptOutcome::Retry {
            status: Some(status),
            retry_after,
        },
        _ => AttemptOutcome::Permanent {
            status: Some(status),
        },
    }
}

fn parse_retry_after(headers: &ureq::http::HeaderMap) -> Option<Duration> {
    let value = headers.get("Retry-After")?.to_str().ok()?.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let seconds = value.bytes().fold(0_u64, |current, byte| {
        current
            .saturating_mul(10)
            .saturating_add(u64::from(byte - b'0'))
    });
    Some(Duration::from_secs(seconds.min(MAX_RETRY_DELAY.as_secs())))
}

#[derive(Clone, Copy)]
struct RetryPolicy {
    schedule: &'static [Duration],
}

impl RetryPolicy {
    const fn production() -> Self {
        Self {
            schedule: &RETRY_DELAYS,
        }
    }

    fn delay(
        self,
        retry_index: usize,
        jitter_seed: u64,
        retry_after: Option<Duration>,
    ) -> Duration {
        let base = self.schedule[retry_index.min(self.schedule.len() - 1)];
        let mixed = mix64(jitter_seed ^ retry_index as u64);
        let jitter_percent = 80_u128 + u128::from(mixed % 41);
        let jittered_ms = base.as_millis().saturating_mul(jitter_percent) / 100;
        let retry_after_ms = retry_after
            .unwrap_or(Duration::ZERO)
            .min(MAX_RETRY_DELAY)
            .as_millis();
        let bounded_ms = jittered_ms
            .max(retry_after_ms)
            .min(MAX_RETRY_DELAY.as_millis());
        Duration::from_millis(u64::try_from(bounded_ms).unwrap_or(u64::MAX))
    }
}

fn upload_loop<T>(
    receiver: Receiver<UploadJob>,
    stop: &AtomicBool,
    mut transport: T,
    retry_policy: RetryPolicy,
) where
    T: UploadTransport,
{
    while !stop.load(Ordering::Acquire) {
        let job = match receiver.recv_timeout(QUEUE_POLL_INTERVAL) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let mut retry_index = 0_usize;
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            match transport.upload(&job) {
                AttemptOutcome::Success => {
                    info!(path = %job.path.display(), "uploaded finalized artifact");
                    break;
                }
                AttemptOutcome::Permanent { status } => {
                    warn!(
                        path = %job.path.display(),
                        ?status,
                        "artifact upload failed permanently"
                    );
                    break;
                }
                AttemptOutcome::Retry {
                    status,
                    retry_after,
                } => {
                    let delay = retry_policy.delay(retry_index, job.jitter_seed, retry_after);
                    retry_index = retry_index.saturating_add(1);
                    warn!(
                        path = %job.path.display(),
                        ?status,
                        retry_ms = delay.as_millis(),
                        "artifact upload failed; retrying"
                    );
                    if wait_for_retry(stop, delay) {
                        return;
                    }
                }
            }
        }
    }
}

/// Returns true when cancellation interrupts the wait.
fn wait_for_retry(stop: &AtomicBool, delay: Duration) -> bool {
    let Some(deadline) = Instant::now().checked_add(delay) else {
        return stop.load(Ordering::Acquire);
    };
    loop {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        thread::park_timeout(remaining);
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58476d1ce4e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        io::{Read, Write},
        net::TcpListener,
        path::Path,
        sync::atomic::AtomicU64,
    };

    use super::*;

    static TEST_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    const NO_DELAY: [Duration; 1] = [Duration::ZERO];
    const LONG_DELAY: [Duration; 1] = [Duration::from_secs(60)];

    struct TestArtifact(PathBuf);

    impl TestArtifact {
        fn new(name: &str, contents: &[u8]) -> Self {
            let sequence = TEST_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "autopiercam-upload-test-{}-{sequence}-{name}",
                std::process::id()
            ));
            std::fs::write(&path, contents).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestArtifact {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn status_and_retry_after_classification_is_explicit_and_bounded() {
        assert_eq!(classify_status(204, None), AttemptOutcome::Success);
        for status in [408, 425, 429, 500, 503, 599] {
            assert!(matches!(
                classify_status(status, None),
                AttemptOutcome::Retry {
                    status: Some(found),
                    ..
                } if found == status
            ));
        }
        for status in [300, 400, 401, 404, 409, 413] {
            assert_eq!(
                classify_status(status, None),
                AttemptOutcome::Permanent {
                    status: Some(status)
                }
            );
        }

        let mut headers = ureq::http::HeaderMap::new();
        headers.insert(
            "Retry-After",
            HeaderValue::from_static("999999999999999999999"),
        );
        assert_eq!(parse_retry_after(&headers), Some(MAX_RETRY_DELAY));
        headers.insert(
            "Retry-After",
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn jitter_is_deterministic_and_capped() {
        let policy = RetryPolicy::production();
        let first = policy.delay(0, 42, None);
        assert_eq!(first, policy.delay(0, 42, None));
        assert!((Duration::from_millis(800)..=Duration::from_millis(1_200)).contains(&first));
        assert_eq!(
            policy.delay(99, 42, Some(Duration::from_secs(999))),
            MAX_RETRY_DELAY
        );
    }

    struct BlockingTransport {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        calls: Arc<AtomicU64>,
    }

    impl UploadTransport for BlockingTransport {
        fn upload(&mut self, _job: &UploadJob) -> AttemptOutcome {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            AttemptOutcome::Success
        }
    }

    #[test]
    fn queue_is_bounded_and_shutdown_abandons_pending_jobs() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let calls = Arc::new(AtomicU64::new(0));
        let transport = BlockingTransport {
            entered: entered_tx,
            release: release_rx,
            calls: Arc::clone(&calls),
        };
        let (mut worker, sink) = UploadWorker::start_with_transport(
            transport,
            RetryPolicy {
                schedule: &NO_DELAY,
            },
            1,
        )
        .unwrap();

        assert_eq!(
            sink.try_enqueue(PathBuf::from("first.jpg")),
            UploadEnqueueResult::Queued
        );
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            sink.try_enqueue(PathBuf::from("second.jpg")),
            UploadEnqueueResult::Queued
        );
        assert_eq!(
            sink.try_enqueue(PathBuf::from("third.jpg")),
            UploadEnqueueResult::QueueFull
        );

        // Set cancellation before releasing the active request. The queued
        // second job must be abandoned rather than started during shutdown.
        worker.stop.store(true, Ordering::Release);
        release_tx.send(()).unwrap();
        worker.stop_and_join_inner().unwrap();
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    struct ScriptedTransport {
        outcomes: VecDeque<AttemptOutcome>,
        attempts: mpsc::Sender<AttemptOutcome>,
    }

    impl UploadTransport for ScriptedTransport {
        fn upload(&mut self, _job: &UploadJob) -> AttemptOutcome {
            let outcome = self.outcomes.pop_front().unwrap_or(AttemptOutcome::Success);
            self.attempts.send(outcome).unwrap();
            outcome
        }
    }

    #[test]
    fn transient_failure_retries_the_same_job() {
        let retry = AttemptOutcome::Retry {
            status: Some(503),
            retry_after: Some(Duration::ZERO),
        };
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let transport = ScriptedTransport {
            outcomes: VecDeque::from([retry, AttemptOutcome::Success]),
            attempts: attempt_tx,
        };
        let (worker, sink) = UploadWorker::start_with_transport(
            transport,
            RetryPolicy {
                schedule: &NO_DELAY,
            },
            1,
        )
        .unwrap();
        assert_eq!(
            sink.try_enqueue(PathBuf::from("retry.jpg")),
            UploadEnqueueResult::Queued
        );
        assert_eq!(
            attempt_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            retry
        );
        assert_eq!(
            attempt_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            AttemptOutcome::Success
        );
        worker.stop_and_join().unwrap();
    }

    struct AlwaysRetry {
        entered: mpsc::Sender<()>,
    }

    impl UploadTransport for AlwaysRetry {
        fn upload(&mut self, _job: &UploadJob) -> AttemptOutcome {
            self.entered.send(()).unwrap();
            AttemptOutcome::Retry {
                status: None,
                retry_after: None,
            }
        }
    }

    #[test]
    fn shutdown_interrupts_a_long_backoff() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (worker, sink) = UploadWorker::start_with_transport(
            AlwaysRetry {
                entered: entered_tx,
            },
            RetryPolicy {
                schedule: &LONG_DELAY,
            },
            1,
        )
        .unwrap();
        assert_eq!(
            sink.try_enqueue(PathBuf::from("offline.jpg")),
            UploadEnqueueResult::Queued
        );
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        worker.stop_and_join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn enqueue_rejects_a_path_without_a_file_name() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let sink = UploadSink { sender };
        assert_eq!(
            sink.try_enqueue(PathBuf::new()),
            UploadEnqueueResult::InvalidArtifactPath
        );
    }

    #[test]
    fn loopback_put_streams_exact_file_and_contract_headers() {
        let jpeg = b"\xff\xd8autopiercam-test-jpeg\xff\xd9";
        let artifact = TestArtifact::new("frame.jpg", jpeg);
        let expected_file_name = artifact
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || read_one_request(listener));

        let endpoint = format!("http://{address}/camera/latest?site=pier")
            .parse::<Uri>()
            .unwrap();
        let authorization = bearer_authorization("contract-secret").unwrap();
        let options = UploadOptions::new(endpoint, Some(authorization), 1);
        let mut transport = UreqTransport::new(options);
        let job = UploadJob::new(artifact.path().to_path_buf()).unwrap();
        assert_eq!(transport.upload(&job), AttemptOutcome::Success);

        let request = server.join().unwrap();
        assert_eq!(
            request.request_line,
            "PUT /camera/latest?site=pier HTTP/1.1"
        );
        assert_eq!(request.headers["content-type"], "image/jpeg");
        assert_eq!(request.headers["content-length"], jpeg.len().to_string());
        assert_eq!(
            request.headers["x-autopiercam-filename"],
            expected_file_name
        );
        assert_eq!(
            request.headers["idempotency-key"],
            format!("autopiercam-{expected_file_name}")
        );
        assert_eq!(request.headers["authorization"], "Bearer contract-secret");
        assert_eq!(request.body, jpeg);
    }

    struct CapturedRequest {
        request_line: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    fn read_one_request(listener: TcpListener) -> CapturedRequest {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 1_024];
            let received = stream.read(&mut chunk).unwrap();
            assert_ne!(received, 0, "request ended before its headers");
            bytes.extend_from_slice(&chunk[..received]);
            assert!(bytes.len() <= 64 * 1_024, "request headers were too large");
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let header_text = std::str::from_utf8(&bytes[..header_end]).unwrap();
        let mut lines = header_text.split("\r\n");
        let request_line = lines.next().unwrap().to_owned();
        let headers = lines
            .filter(|line| !line.is_empty())
            .map(|line| {
                let (name, value) = line.split_once(':').unwrap();
                (name.to_ascii_lowercase(), value.trim().to_owned())
            })
            .collect::<HashMap<_, _>>();
        if headers
            .get("expect")
            .is_some_and(|value| value.eq_ignore_ascii_case("100-continue"))
        {
            stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").unwrap();
        }
        let content_length = headers["content-length"].parse::<usize>().unwrap();
        while bytes.len() - header_end < content_length {
            let mut chunk = [0_u8; 4_096];
            let received = stream.read(&mut chunk).unwrap();
            assert_ne!(received, 0, "request ended before its body");
            bytes.extend_from_slice(&chunk[..received]);
        }
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        CapturedRequest {
            request_line,
            headers,
            body: bytes[header_end..header_end + content_length].to_vec(),
        }
    }
}
