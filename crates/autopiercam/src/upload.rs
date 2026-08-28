use std::{
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use thiserror::Error;
use tracing::{info, warn};
use ureq::{
    Agent,
    http::{
        HeaderValue, Uri,
        header::{AUTHORIZATION, CONTENT_LENGTH},
    },
    tls::{RootCerts, TlsConfig},
};

#[path = "upload_store.rs"]
mod upload_store;

use upload_store::{
    ArtifactVerification, ClaimedUpload, OpenedClaimedArtifact, RecordDisposition, UploadStore,
    UploadStoreError, UploadStoreSnapshot, open_claimed_artifact,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_IDLE_POLL: Duration = Duration::from_millis(100);
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

const MISSING_ARTIFACT_ERROR: &str = "artifact file is missing";
const UNOPENABLE_ARTIFACT_ERROR: &str = "artifact file could not be opened or verified";
const SYMLINK_ARTIFACT_ERROR: &str = "artifact path became a symbolic link";
const NON_FILE_ARTIFACT_ERROR: &str = "artifact path is not a regular file";
const ARTIFACT_SIZE_CHANGED_ERROR: &str = "artifact file size changed after it was recorded";
const ARTIFACT_CONTENT_CHANGED_ERROR: &str = "artifact file content changed after it was recorded";
const INVALID_HEADER_ERROR: &str = "artifact metadata cannot be represented as HTTP headers";
const TRANSPORT_ERROR: &str = "HTTP transport failed";
const RETRYABLE_HTTP_ERROR: &str = "HTTP endpoint requested a retry";
const PERMANENT_HTTP_ERROR: &str = "HTTP endpoint rejected the artifact";
const RETRY_TIME_ERROR: &str = "retry time cannot be represented safely";

/// Complete HTTP configuration for one durable upload worker.
pub(crate) struct UploadOptions {
    endpoint: Uri,
    authorization: Option<HeaderValue>,
    /// Bounds coalesced wake hints. SQLite, not this channel, owns the queue.
    wake_capacity: usize,
}

impl UploadOptions {
    pub(crate) fn new(
        endpoint: Uri,
        mut authorization: Option<HeaderValue>,
        wake_capacity: usize,
    ) -> Self {
        if let Some(value) = &mut authorization {
            value.set_sensitive(true);
        }
        Self {
            endpoint,
            authorization,
            wake_capacity,
        }
    }
}

/// Constructs a sensitive `Authorization: Bearer ...` value without exposing
/// rejected token bytes through this module's errors or logs.
pub(crate) fn bearer_authorization(
    token: &str,
) -> Result<HeaderValue, ureq::http::header::InvalidHeaderValue> {
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))?;
    value.set_sensitive(true);
    Ok(value)
}

/// A callback receives these snapshots in ledger mutation order. Callbacks
/// must be fast and must not call back into the same upload worker or sink.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct UploadTelemetry {
    pub(crate) pending: u64,
    pub(crate) active: u64,
    pub(crate) retrying: u64,
    pub(crate) completed: u64,
    pub(crate) permanently_failed: u64,
    pub(crate) last_success_unix_ms: Option<u64>,
    pub(crate) last_failure_unix_ms: Option<u64>,
    pub(crate) last_error: Option<String>,
}

impl From<UploadStoreSnapshot> for UploadTelemetry {
    fn from(snapshot: UploadStoreSnapshot) -> Self {
        Self {
            pending: snapshot.counts.pending,
            active: snapshot.counts.in_progress,
            retrying: snapshot.counts.retrying,
            completed: snapshot.counts.completed,
            permanently_failed: snapshot.counts.permanently_failed,
            last_success_unix_ms: snapshot.last_success_at_unix_ms,
            last_failure_unix_ms: snapshot.last_failure_at_unix_ms,
            last_error: snapshot.last_error,
        }
    }
}

pub(crate) type UploadObserver = Arc<dyn Fn(UploadTelemetry) + Send + Sync + 'static>;

#[derive(Debug, Error)]
pub(crate) enum UploadError {
    #[error(transparent)]
    Store(#[from] UploadStoreError),

    #[error("upload wake capacity must be greater than zero")]
    InvalidWakeCapacity,

    #[error("the upload ledger lock was poisoned")]
    StoreLockPoisoned,

    #[error("the upload telemetry publication lock was poisoned")]
    PublishLockPoisoned,

    #[error("the system clock is before the Unix epoch")]
    ClockBeforeUnixEpoch,

    #[error("the system clock cannot be represented in milliseconds")]
    ClockOutOfRange,

    #[error("the upload telemetry observer panicked")]
    ObserverPanicked,

    #[error("could not start the upload worker thread")]
    ThreadStart(#[source] io::Error),

    #[error("the upload worker thread panicked")]
    WorkerPanicked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UploadEnqueueResult {
    Recorded,
    AlreadyRecorded,
    /// The artifact was recorded durably, but the worker has stopped and will
    /// not attempt it until upload service is restarted.
    WorkerStopped,
}

type SharedStore = Arc<Mutex<UploadStore>>;
type PublishLock = Arc<Mutex<()>>;

/// Cloneable liveness view shared with the owner of an upload sink.
///
/// This becomes stopped as soon as shutdown is requested or the worker exits,
/// including exits caused by a fatal worker-side error.
#[derive(Clone, Debug)]
pub(crate) struct UploadHealth {
    stopped: Arc<AtomicBool>,
}

impl UploadHealth {
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
pub(crate) struct UploadSink {
    store: SharedStore,
    publish_lock: PublishLock,
    wake_sender: SyncSender<()>,
    stop: Arc<AtomicBool>,
    observer: UploadObserver,
    clock: Arc<dyn UploadClock>,
}

impl UploadSink {
    pub(crate) fn health(&self) -> UploadHealth {
        UploadHealth {
            stopped: Arc::clone(&self.stop),
        }
    }

    /// Records a finalized artifact durably before attempting a nonblocking
    /// wake. `Full` is success because another wake is already pending and the
    /// worker always drains all due ledger rows.
    pub(crate) fn try_enqueue(&self, path: PathBuf) -> Result<UploadEnqueueResult, UploadError> {
        let result = self.record_and_publish(&path);
        let disposition = match result {
            Ok(disposition) => disposition,
            Err(error) => {
                self.signal_stop();
                return Err(error);
            }
        };

        if self.health().is_stopped() {
            return Ok(UploadEnqueueResult::WorkerStopped);
        }
        match self.wake_sender.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => Ok(match disposition {
                RecordDisposition::Inserted => UploadEnqueueResult::Recorded,
                RecordDisposition::AlreadyRecorded => UploadEnqueueResult::AlreadyRecorded,
            }),
            Err(TrySendError::Disconnected(())) => {
                self.stop.store(true, Ordering::Release);
                Ok(UploadEnqueueResult::WorkerStopped)
            }
        }
    }

    fn record_and_publish(&self, path: &Path) -> Result<RecordDisposition, UploadError> {
        let now = self.clock.now_unix_ms()?;
        let _publication = lock_publish(&self.publish_lock)?;
        let (disposition, telemetry) = {
            let mut store = lock_store(&self.store)?;
            let disposition = store.record_artifact(path, now)?.disposition;
            let telemetry = UploadTelemetry::from(store.snapshot()?);
            (disposition, telemetry)
        };
        notify_observer(&self.observer, telemetry)?;
        Ok(disposition)
    }

    fn signal_stop(&self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.wake_sender.try_send(());
    }
}

/// Owns the single HTTP thread paired with an [`UploadSink`].
pub(crate) struct UploadWorker {
    stop: Arc<AtomicBool>,
    wake_sender: SyncSender<()>,
    thread: Option<JoinHandle<Result<(), UploadError>>>,
}

impl UploadWorker {
    pub(crate) fn start(
        options: UploadOptions,
        database_path: &Path,
        capture_directory: &Path,
        observer: UploadObserver,
    ) -> Result<(Self, UploadSink), UploadError> {
        let wake_capacity = options.wake_capacity;
        let destination = options.endpoint.to_string();
        let transport = UreqTransport::new(options);
        Self::start_with_transport(
            transport,
            RetryPolicy::production(),
            wake_capacity,
            database_path,
            capture_directory,
            &destination,
            observer,
            Arc::new(SystemClock),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_with_transport<T>(
        transport: T,
        retry_policy: RetryPolicy,
        wake_capacity: usize,
        database_path: &Path,
        capture_directory: &Path,
        destination: &str,
        observer: UploadObserver,
        clock: Arc<dyn UploadClock>,
    ) -> Result<(Self, UploadSink), UploadError>
    where
        T: UploadTransport,
    {
        if wake_capacity == 0 {
            return Err(UploadError::InvalidWakeCapacity);
        }

        let now = clock.now_unix_ms()?;
        let mut store = UploadStore::open(database_path, capture_directory, destination)?;
        store.reconcile_capture_directory(now)?;
        notify_observer(&observer, UploadTelemetry::from(store.snapshot()?))?;

        let store = Arc::new(Mutex::new(store));
        let publish_lock = Arc::new(Mutex::new(()));
        let (wake_sender, wake_receiver) = mpsc::sync_channel(wake_capacity);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_store = Arc::clone(&store);
        let worker_publish_lock = Arc::clone(&publish_lock);
        let worker_stop = Arc::clone(&stop);
        let worker_observer = Arc::clone(&observer);
        let worker_clock = Arc::clone(&clock);
        let thread = thread::Builder::new()
            .name("autopiercam-upload".to_owned())
            .spawn(move || {
                let _stop_on_exit = StopOnDrop(Arc::clone(&worker_stop));
                upload_loop(
                    wake_receiver,
                    &worker_stop,
                    &worker_store,
                    &worker_publish_lock,
                    transport,
                    retry_policy,
                    &worker_observer,
                    &worker_clock,
                )
            })
            .map_err(UploadError::ThreadStart)?;

        Ok((
            Self {
                stop: Arc::clone(&stop),
                wake_sender: wake_sender.clone(),
                thread: Some(thread),
            },
            UploadSink {
                store,
                publish_lock,
                wake_sender,
                stop,
                observer,
                clock,
            },
        ))
    }

    pub(crate) fn stop_and_join(mut self) -> Result<(), UploadError> {
        self.stop_and_join_inner()
    }

    fn stop_and_join_inner(&mut self) -> Result<(), UploadError> {
        self.stop.store(true, Ordering::Release);
        let _ = self.wake_sender.try_send(());
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread.join().map_err(|_| UploadError::WorkerPanicked)?
    }
}

impl Drop for UploadWorker {
    fn drop(&mut self) {
        if let Err(error) = self.stop_and_join_inner() {
            warn!(%error, "upload worker did not shut down cleanly");
        }
    }
}

struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

trait UploadClock: Send + Sync + 'static {
    fn now_unix_ms(&self) -> Result<u64, UploadError>;
}

struct SystemClock;

impl UploadClock for SystemClock {
    fn now_unix_ms(&self) -> Result<u64, UploadError> {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| UploadError::ClockBeforeUnixEpoch)?;
        u64::try_from(elapsed.as_millis()).map_err(|_| UploadError::ClockOutOfRange)
    }
}

trait UploadTransport: Send + 'static {
    fn upload(&mut self, claim: &ClaimedUpload) -> AttemptOutcome;
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
    fn upload(&mut self, claim: &ClaimedUpload) -> AttemptOutcome {
        let file = match open_claimed_artifact(claim) {
            Ok(OpenedClaimedArtifact::Verified(file)) => file,
            Ok(OpenedClaimedArtifact::Rejected(ArtifactVerification::Missing)) => {
                return AttemptOutcome::permanent(None, MISSING_ARTIFACT_ERROR);
            }
            Ok(OpenedClaimedArtifact::Rejected(ArtifactVerification::Symlink)) => {
                return AttemptOutcome::permanent(None, SYMLINK_ARTIFACT_ERROR);
            }
            Ok(OpenedClaimedArtifact::Rejected(ArtifactVerification::NotRegularFile)) => {
                return AttemptOutcome::permanent(None, NON_FILE_ARTIFACT_ERROR);
            }
            Ok(OpenedClaimedArtifact::Rejected(ArtifactVerification::SizeMismatch { .. })) => {
                return AttemptOutcome::permanent(None, ARTIFACT_SIZE_CHANGED_ERROR);
            }
            Ok(OpenedClaimedArtifact::Rejected(ArtifactVerification::Sha256Mismatch {
                ..
            })) => {
                return AttemptOutcome::permanent(None, ARTIFACT_CONTENT_CHANGED_ERROR);
            }
            #[cfg(test)]
            Ok(OpenedClaimedArtifact::Rejected(ArtifactVerification::Verified)) => {
                return AttemptOutcome::retry(None, None, UNOPENABLE_ARTIFACT_ERROR);
            }
            Err(_) => return AttemptOutcome::retry(None, None, UNOPENABLE_ARTIFACT_ERROR),
        };
        let file_name = match HeaderValue::from_str(&claim.filename) {
            Ok(value) => value,
            Err(_) => return AttemptOutcome::permanent(None, INVALID_HEADER_ERROR),
        };
        let idempotency_key = match HeaderValue::from_str(&claim.idempotency_key) {
            Ok(value) => value,
            Err(_) => return AttemptOutcome::permanent(None, INVALID_HEADER_ERROR),
        };

        let mut request = self
            .agent
            .put(self.endpoint.clone())
            .content_type("image/jpeg")
            .header(CONTENT_LENGTH, claim.file_size.to_string())
            .header("X-AutoPierCam-Filename", file_name)
            .header("Idempotency-Key", idempotency_key);
        if let Some(authorization) = &self.authorization {
            request = request.header(AUTHORIZATION, authorization.clone());
        }

        match request.send(file) {
            Ok(response) => {
                let status = response.status().as_u16();
                classify_status(status, parse_retry_after(response.headers()))
            }
            Err(ureq::Error::StatusCode(status)) => classify_status(status, None),
            Err(_) => AttemptOutcome::retry(None, None, TRANSPORT_ERROR),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttemptOutcome {
    Success {
        status: u16,
    },
    Retry {
        status: Option<u16>,
        retry_after: Option<Duration>,
        error: &'static str,
    },
    Permanent {
        status: Option<u16>,
        error: &'static str,
    },
}

impl AttemptOutcome {
    const fn retry(
        status: Option<u16>,
        retry_after: Option<Duration>,
        error: &'static str,
    ) -> Self {
        Self::Retry {
            status,
            retry_after,
            error,
        }
    }

    const fn permanent(status: Option<u16>, error: &'static str) -> Self {
        Self::Permanent { status, error }
    }
}

fn classify_status(status: u16, retry_after: Option<Duration>) -> AttemptOutcome {
    match status {
        200..=299 => AttemptOutcome::Success { status },
        408 | 425 | 429 | 500..=599 => {
            AttemptOutcome::retry(Some(status), retry_after, RETRYABLE_HTTP_ERROR)
        }
        _ => AttemptOutcome::permanent(Some(status), PERMANENT_HTTP_ERROR),
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
        attempt_count: u64,
        jitter_seed: u64,
        retry_after: Option<Duration>,
    ) -> Duration {
        debug_assert!(!self.schedule.is_empty());
        let retry_index = attempt_count.saturating_sub(1);
        let schedule_index = usize::try_from(retry_index)
            .unwrap_or(usize::MAX)
            .min(self.schedule.len() - 1);
        let base = self.schedule[schedule_index];
        let mixed = mix64(jitter_seed ^ retry_index);
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

#[allow(clippy::too_many_arguments)]
fn upload_loop<T>(
    wake_receiver: Receiver<()>,
    stop: &AtomicBool,
    shared_store: &SharedStore,
    publish_lock: &PublishLock,
    mut transport: T,
    retry_policy: RetryPolicy,
    observer: &UploadObserver,
    clock: &Arc<dyn UploadClock>,
) -> Result<(), UploadError>
where
    T: UploadTransport,
{
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }

        let claim_time = clock.now_unix_ms()?;
        let (claim, next_due) =
            claim_and_publish(shared_store, publish_lock, observer, claim_time)?;
        let Some(claim) = claim else {
            match wake_receiver.recv_timeout(wait_until_due(next_due, claim_time)) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        };
        if stop.load(Ordering::Acquire) {
            let _publication = lock_publish(publish_lock)?;
            let telemetry = {
                let mut store = lock_store(shared_store)?;
                store.release_claim(&claim, claim_time)?;
                UploadTelemetry::from(store.snapshot()?)
            };
            notify_observer(observer, telemetry)?;
            return Ok(());
        }

        // No database mutex is held across verification or the bounded HTTP
        // request. Once claimed, the outcome is persisted even if stop races in.
        let outcome = transport.upload(&claim);
        let (finished_at, clock_error) = match clock.now_unix_ms() {
            Ok(now) => (now, None),
            Err(error) => (claim_time, Some(error)),
        };
        persist_outcome_and_publish(
            shared_store,
            publish_lock,
            observer,
            &claim,
            outcome,
            finished_at,
            retry_policy,
        )?;

        if let Some(error) = clock_error {
            return Err(error);
        }
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
    }
}

fn claim_and_publish(
    shared_store: &SharedStore,
    publish_lock: &PublishLock,
    observer: &UploadObserver,
    now: u64,
) -> Result<(Option<ClaimedUpload>, Option<u64>), UploadError> {
    let _publication = lock_publish(publish_lock)?;
    let (claim, snapshot) = {
        let mut store = lock_store(shared_store)?;
        let claim = store.claim_due(now)?;
        let snapshot = store.snapshot()?;
        (claim, snapshot)
    };
    let next_due = snapshot.next_due_at_unix_ms;
    if claim.is_some() {
        notify_observer(observer, UploadTelemetry::from(snapshot))?;
    }
    Ok((claim, next_due))
}

#[allow(clippy::too_many_arguments)]
fn persist_outcome_and_publish(
    shared_store: &SharedStore,
    publish_lock: &PublishLock,
    observer: &UploadObserver,
    claim: &ClaimedUpload,
    outcome: AttemptOutcome,
    finished_at: u64,
    retry_policy: RetryPolicy,
) -> Result<(), UploadError> {
    let _publication = lock_publish(publish_lock)?;
    let telemetry = {
        let mut store = lock_store(shared_store)?;
        match outcome {
            AttemptOutcome::Success { status } => {
                store.mark_completed(claim, finished_at, status)?;
                info!(
                    path = %claim.artifact_path.display(),
                    attempt = claim.attempt_count,
                    status,
                    "uploaded finalized artifact"
                );
            }
            AttemptOutcome::Permanent { status, error } => {
                store.mark_permanently_failed(claim, finished_at, status, Some(error))?;
                warn!(
                    path = %claim.artifact_path.display(),
                    attempt = claim.attempt_count,
                    ?status,
                    error,
                    "artifact upload failed permanently"
                );
            }
            AttemptOutcome::Retry {
                status,
                retry_after,
                error,
            } => {
                let delay = retry_policy.delay(
                    claim.attempt_count,
                    fnv1a(claim.idempotency_key.as_bytes()),
                    retry_after,
                );
                let delay_ms =
                    u64::try_from(delay.as_millis()).map_err(|_| UploadError::ClockOutOfRange)?;
                let Some(next_attempt_at) = finished_at.checked_add(delay_ms) else {
                    store.mark_permanently_failed(
                        claim,
                        finished_at,
                        status,
                        Some(RETRY_TIME_ERROR),
                    )?;
                    return Err(UploadError::ClockOutOfRange);
                };
                store.mark_retrying(claim, finished_at, next_attempt_at, status, Some(error))?;
                warn!(
                    path = %claim.artifact_path.display(),
                    attempt = claim.attempt_count,
                    ?status,
                    retry_ms = delay.as_millis(),
                    error,
                    "artifact upload failed; retry scheduled"
                );
            }
        }
        UploadTelemetry::from(store.snapshot()?)
    };
    notify_observer(observer, telemetry)
}

fn lock_store(store: &SharedStore) -> Result<MutexGuard<'_, UploadStore>, UploadError> {
    store.lock().map_err(|_| UploadError::StoreLockPoisoned)
}

fn lock_publish(publish_lock: &PublishLock) -> Result<MutexGuard<'_, ()>, UploadError> {
    publish_lock
        .lock()
        .map_err(|_| UploadError::PublishLockPoisoned)
}

fn notify_observer(
    observer: &UploadObserver,
    telemetry: UploadTelemetry,
) -> Result<(), UploadError> {
    catch_unwind(AssertUnwindSafe(|| observer(telemetry)))
        .map_err(|_| UploadError::ObserverPanicked)
}

fn wait_until_due(next_due: Option<u64>, now: u64) -> Duration {
    let until_due = next_due
        .map(|due| Duration::from_millis(due.saturating_sub(now)))
        .unwrap_or(MAX_IDLE_POLL);
    until_due.min(MAX_IDLE_POLL)
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
        collections::HashMap,
        io::{Read, Write},
        net::TcpListener,
        str::FromStr,
        sync::atomic::AtomicU64,
    };

    use super::*;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    const LONG_DELAY: [Duration; 1] = [Duration::from_secs(60)];
    const TEST_DESTINATION: &str = "https://upload.example.test/camera/latest";

    struct TestEnvironment {
        root: PathBuf,
        capture: PathBuf,
        database: PathBuf,
    }

    impl TestEnvironment {
        fn new() -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "autopiercam-upload-integration-test-{}-{sequence}",
                std::process::id()
            ));
            let capture = root.join("captures");
            std::fs::create_dir_all(&capture).unwrap();
            let database = root.join("uploads.sqlite3");
            Self {
                root,
                capture,
                database,
            }
        }

        fn artifact(&self, sequence: u64, contents: &[u8]) -> PathBuf {
            let path = self
                .capture
                .join(format!("frame-1700000000-123-{sequence:06}.jpg"));
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TestEnvironment {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    struct ManualClock(AtomicU64);

    impl ManualClock {
        fn new(now: u64) -> Self {
            Self(AtomicU64::new(now))
        }

        fn set(&self, now: u64) {
            self.0.store(now, Ordering::Release);
        }
    }

    impl UploadClock for ManualClock {
        fn now_unix_ms(&self) -> Result<u64, UploadError> {
            Ok(self.0.load(Ordering::Acquire))
        }
    }

    fn observer_channel() -> (UploadObserver, mpsc::Receiver<UploadTelemetry>) {
        let (sender, receiver) = mpsc::channel();
        let observer: UploadObserver = Arc::new(move |telemetry| {
            let _ = sender.send(telemetry);
        });
        (observer, receiver)
    }

    fn receive_until(
        receiver: &mpsc::Receiver<UploadTelemetry>,
        predicate: impl Fn(&UploadTelemetry) -> bool,
    ) -> UploadTelemetry {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let telemetry = receiver.recv_timeout(remaining).unwrap();
            if predicate(&telemetry) {
                return telemetry;
            }
        }
    }

    #[test]
    fn fatal_worker_error_stops_health_while_later_intents_remain_durable() {
        let environment = TestEnvironment::new();
        let failed_once = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observer_failed_once = Arc::clone(&failed_once);
        let observer: UploadObserver = Arc::new(move |telemetry| {
            if telemetry.active != 0 && !observer_failed_once.swap(true, Ordering::AcqRel) {
                panic!("injected worker-side observer failure");
            }
        });
        let (worker, sink) = UploadWorker::start_with_transport(
            ReportingSuccessTransport {
                attempt: mpsc::channel().0,
            },
            RetryPolicy::production(),
            1,
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            observer,
            Arc::new(ManualClock::new(1_000)),
        )
        .unwrap();
        let health = sink.health();
        assert!(!health.is_stopped());

        assert_eq!(
            sink.try_enqueue(environment.artifact(1, b"first durable intent"))
                .unwrap(),
            UploadEnqueueResult::Recorded
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !health.is_stopped() {
            assert!(
                std::time::Instant::now() < deadline,
                "worker health did not observe the fatal exit"
            );
            std::thread::yield_now();
        }

        assert_eq!(
            sink.try_enqueue(environment.artifact(2, b"recorded after worker exit"))
                .unwrap(),
            UploadEnqueueResult::WorkerStopped
        );
        assert_eq!(
            lock_store(&sink.store)
                .unwrap()
                .snapshot()
                .unwrap()
                .counts
                .total(),
            2
        );
        assert!(matches!(
            worker.stop_and_join(),
            Err(UploadError::ObserverPanicked)
        ));
        drop(sink);

        let reopened = UploadStore::open(
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
        )
        .unwrap();
        let recovered = reopened.snapshot().unwrap();
        assert_eq!(recovered.counts.pending, 2);
        assert_eq!(recovered.counts.total(), 2);
    }

    struct FirstBlockingTransport {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        attempts: Arc<AtomicU64>,
    }

    impl UploadTransport for FirstBlockingTransport {
        fn upload(&mut self, _claim: &ClaimedUpload) -> AttemptOutcome {
            let attempt = self.attempts.fetch_add(1, Ordering::AcqRel);
            if attempt == 0 {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            AttemptOutcome::Success { status: 204 }
        }
    }

    #[test]
    fn full_wake_channel_never_loses_durable_intent() {
        let environment = TestEnvironment::new();
        let (observer, telemetry) = observer_channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let attempts = Arc::new(AtomicU64::new(0));
        let transport = FirstBlockingTransport {
            entered: entered_tx,
            release: release_rx,
            attempts: Arc::clone(&attempts),
        };
        let (worker, sink) = UploadWorker::start_with_transport(
            transport,
            RetryPolicy::production(),
            1,
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            observer,
            Arc::new(ManualClock::new(1_000)),
        )
        .unwrap();

        assert_eq!(
            sink.try_enqueue(environment.artifact(1, b"first")).unwrap(),
            UploadEnqueueResult::Recorded
        );
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // Occupy the sole advisory wake slot. These records must still succeed.
        assert!(matches!(
            sink.wake_sender.try_send(()),
            Ok(()) | Err(TrySendError::Full(()))
        ));
        assert_eq!(
            sink.try_enqueue(environment.artifact(2, b"second"))
                .unwrap(),
            UploadEnqueueResult::Recorded
        );
        assert_eq!(
            sink.try_enqueue(environment.artifact(3, b"third")).unwrap(),
            UploadEnqueueResult::Recorded
        );
        assert_eq!(
            lock_store(&sink.store)
                .unwrap()
                .snapshot()
                .unwrap()
                .counts
                .total(),
            3
        );

        release_tx.send(()).unwrap();
        let final_state = receive_until(&telemetry, |value| value.completed == 3);
        assert_eq!(final_state.pending, 0);
        assert_eq!(attempts.load(Ordering::Acquire), 3);
        worker.stop_and_join().unwrap();
    }

    struct BlockingRetryTransport {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl UploadTransport for BlockingRetryTransport {
        fn upload(&mut self, _claim: &ClaimedUpload) -> AttemptOutcome {
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            AttemptOutcome::retry(Some(503), None, RETRYABLE_HTTP_ERROR)
        }
    }

    struct ReportingSuccessTransport {
        attempt: mpsc::Sender<u64>,
    }

    impl UploadTransport for ReportingSuccessTransport {
        fn upload(&mut self, claim: &ClaimedUpload) -> AttemptOutcome {
            self.attempt.send(claim.attempt_count).unwrap();
            AttemptOutcome::Success { status: 204 }
        }
    }

    struct ReportingRetryTransport {
        attempt: mpsc::Sender<u64>,
    }

    impl UploadTransport for ReportingRetryTransport {
        fn upload(&mut self, claim: &ClaimedUpload) -> AttemptOutcome {
            self.attempt.send(claim.attempt_count).unwrap();
            AttemptOutcome::retry(Some(503), None, RETRYABLE_HTTP_ERROR)
        }
    }

    #[test]
    fn shutdown_finishes_current_attempt_and_restart_obeys_persisted_retry() {
        let environment = TestEnvironment::new();
        let manual_clock = Arc::new(ManualClock::new(10_000));
        let (observer, telemetry) = observer_channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (mut worker, sink) = UploadWorker::start_with_transport(
            BlockingRetryTransport {
                entered: entered_tx,
                release: release_rx,
            },
            RetryPolicy {
                schedule: &LONG_DELAY,
            },
            1,
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            observer,
            manual_clock.clone(),
        )
        .unwrap();
        sink.try_enqueue(environment.artifact(10, b"retry me"))
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        // Stop races with the active request. Releasing it must still persist
        // the retry outcome before join returns.
        worker.stop.store(true, Ordering::Release);
        release_tx.send(()).unwrap();
        worker.stop_and_join_inner().unwrap();
        let retrying = receive_until(&telemetry, |value| value.retrying == 1);
        let due = lock_store(&sink.store)
            .unwrap()
            .snapshot()
            .unwrap()
            .next_due_at_unix_ms
            .unwrap();
        assert!(due > 10_000);
        assert_eq!(retrying.active, 0);
        drop(sink);

        manual_clock.set(due);
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let (observer, telemetry) = observer_channel();
        let (worker, sink) = UploadWorker::start_with_transport(
            ReportingSuccessTransport {
                attempt: attempt_tx,
            },
            RetryPolicy {
                schedule: &LONG_DELAY,
            },
            1,
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            observer,
            manual_clock,
        )
        .unwrap();
        assert_eq!(attempt_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 2);
        receive_until(&telemetry, |value| value.completed == 1);
        worker.stop_and_join().unwrap();
        drop(sink);
    }

    #[test]
    fn prolonged_outage_survives_repeated_restarts_until_success() {
        let environment = TestEnvironment::new();
        let artifact = environment.artifact(11, b"retry across several worker lifetimes");
        let manual_clock = Arc::new(ManualClock::new(50_000));
        let mut now = 50_000;
        let mut last_failure_at = 0;

        for expected_attempt in 1..=3 {
            manual_clock.set(now);
            let (attempt_tx, attempt_rx) = mpsc::channel();
            let (observer, telemetry) = observer_channel();
            let (worker, sink) = UploadWorker::start_with_transport(
                ReportingRetryTransport {
                    attempt: attempt_tx,
                },
                RetryPolicy {
                    schedule: &LONG_DELAY,
                },
                1,
                &environment.database,
                &environment.capture,
                TEST_DESTINATION,
                observer,
                manual_clock.clone(),
            )
            .unwrap();
            if expected_attempt == 1 {
                assert_eq!(
                    sink.try_enqueue(artifact.clone()).unwrap(),
                    UploadEnqueueResult::Recorded
                );
            }

            assert_eq!(
                attempt_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                expected_attempt
            );
            let retrying = receive_until(&telemetry, |value| {
                value.active == 0 && value.retrying == 1 && value.last_failure_unix_ms == Some(now)
            });
            assert_eq!(retrying.last_failure_unix_ms, Some(now));
            assert_eq!(retrying.last_error.as_deref(), Some(RETRYABLE_HTTP_ERROR));
            let due = lock_store(&sink.store)
                .unwrap()
                .snapshot()
                .unwrap()
                .next_due_at_unix_ms
                .unwrap();
            assert!(due > now);

            worker.stop_and_join().unwrap();
            drop(sink);
            last_failure_at = now;
            now = due;
        }

        manual_clock.set(now);
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let (observer, telemetry) = observer_channel();
        let (worker, sink) = UploadWorker::start_with_transport(
            ReportingSuccessTransport {
                attempt: attempt_tx,
            },
            RetryPolicy {
                schedule: &LONG_DELAY,
            },
            1,
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            observer,
            manual_clock,
        )
        .unwrap();
        assert_eq!(attempt_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 4);
        let completed = receive_until(&telemetry, |value| value.completed == 1);
        assert_eq!(completed.retrying, 0);
        assert_eq!(completed.last_success_unix_ms, Some(now));
        assert_eq!(completed.last_failure_unix_ms, Some(last_failure_at));
        worker.stop_and_join().unwrap();
        drop(sink);
    }

    #[test]
    fn restart_recovers_an_abandoned_claim() {
        let environment = TestEnvironment::new();
        let artifact = environment.artifact(20, b"recover me");
        let stale_claim = {
            let mut store = UploadStore::open(
                &environment.database,
                &environment.capture,
                TEST_DESTINATION,
            )
            .unwrap();
            store.record_artifact(&artifact, 20_000).unwrap();
            store.claim_due(20_000).unwrap().unwrap()
        };
        assert_eq!(stale_claim.attempt_count, 1);

        let (attempt_tx, attempt_rx) = mpsc::channel();
        let (observer, telemetry) = observer_channel();
        let (worker, sink) = UploadWorker::start_with_transport(
            ReportingSuccessTransport {
                attempt: attempt_tx,
            },
            RetryPolicy::production(),
            1,
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            observer,
            Arc::new(ManualClock::new(20_000)),
        )
        .unwrap();
        assert_eq!(attempt_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 2);
        receive_until(&telemetry, |value| value.completed == 1);
        worker.stop_and_join().unwrap();
        drop(sink);
    }

    #[test]
    fn missing_artifact_becomes_a_safe_permanent_failure() {
        let environment = TestEnvironment::new();
        let missing = environment.artifact(30, b"gone soon");
        {
            let mut store = UploadStore::open(
                &environment.database,
                &environment.capture,
                TEST_DESTINATION,
            )
            .unwrap();
            store.record_artifact(&missing, 30_000).unwrap();
        }
        std::fs::remove_file(&missing).unwrap();

        let endpoint = Uri::from_static("http://127.0.0.1:9/upload");
        let (observer, telemetry) = observer_channel();
        let (worker, sink) = UploadWorker::start_with_transport(
            UreqTransport::new(UploadOptions::new(endpoint, None, 1)),
            RetryPolicy::production(),
            1,
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            observer,
            Arc::new(ManualClock::new(30_000)),
        )
        .unwrap();
        let failed = receive_until(&telemetry, |value| value.permanently_failed == 1);
        assert_eq!(failed.last_error.as_deref(), Some(MISSING_ARTIFACT_ERROR));
        worker.stop_and_join().unwrap();
        drop(sink);
    }

    #[test]
    fn replaced_artifact_becomes_a_safe_permanent_failure() {
        let environment = TestEnvironment::new();
        let artifact = environment.artifact(31, b"original bytes");
        {
            let mut store = UploadStore::open(
                &environment.database,
                &environment.capture,
                TEST_DESTINATION,
            )
            .unwrap();
            store.record_artifact(&artifact, 31_000).unwrap();
        }
        std::fs::remove_file(&artifact).unwrap();
        std::fs::write(&artifact, b"replaced bytes").unwrap();

        let endpoint = Uri::from_static("http://127.0.0.1:9/upload");
        let (observer, telemetry) = observer_channel();
        let (worker, sink) = UploadWorker::start_with_transport(
            UreqTransport::new(UploadOptions::new(endpoint, None, 1)),
            RetryPolicy::production(),
            1,
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
            observer,
            Arc::new(ManualClock::new(31_000)),
        )
        .unwrap();
        let failed = receive_until(&telemetry, |value| value.permanently_failed == 1);
        assert_eq!(
            failed.last_error.as_deref(),
            Some(ARTIFACT_CONTENT_CHANGED_ERROR)
        );
        worker.stop_and_join().unwrap();
        drop(sink);
    }

    #[test]
    fn status_retry_after_and_jitter_are_explicit_and_bounded() {
        assert_eq!(
            classify_status(204, None),
            AttemptOutcome::Success { status: 204 }
        );
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
            assert!(matches!(
                classify_status(status, None),
                AttemptOutcome::Permanent {
                    status: Some(found),
                    ..
                } if found == status
            ));
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

        let policy = RetryPolicy::production();
        let first = policy.delay(1, 42, None);
        assert_eq!(first, policy.delay(1, 42, None));
        assert!((Duration::from_millis(800)..=Duration::from_millis(1_200)).contains(&first));
        assert_eq!(
            policy.delay(100, 42, Some(Duration::from_secs(999))),
            MAX_RETRY_DELAY
        );
    }

    #[test]
    fn loopback_put_streams_exact_verified_file_and_headers() {
        let environment = TestEnvironment::new();
        let jpeg = b"\xff\xd8autopiercam-test-jpeg\xff\xd9";
        let artifact = environment.artifact(40, jpeg);
        let expected_file_name = artifact.file_name().unwrap().to_str().unwrap().to_owned();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || read_one_request(listener));

        let endpoint = Uri::from_str(&format!("http://{address}/camera/latest?site=pier")).unwrap();
        let authorization = bearer_authorization("contract-secret").unwrap();
        let mut transport =
            UreqTransport::new(UploadOptions::new(endpoint, Some(authorization), 1));
        let mut store = UploadStore::open(
            &environment.database,
            &environment.capture,
            TEST_DESTINATION,
        )
        .unwrap();
        store.record_artifact(&artifact, 40_000).unwrap();
        let claim = store.claim_due(40_000).unwrap().unwrap();
        let expected_idempotency_key = claim.idempotency_key.clone();
        assert_eq!(
            transport.upload(&claim),
            AttemptOutcome::Success { status: 204 }
        );

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
        assert_eq!(request.headers["idempotency-key"], expected_idempotency_key);
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
