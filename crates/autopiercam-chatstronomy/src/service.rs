use crate::{
    origin::{HubOrigin, TransportPolicy},
    protocol::Secret,
    vault::{self, CredentialStore, OsCredentialStore},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};
use tokio::sync::watch;

pub use crate::vault::CredentialStore as SecretStore;

/// A frame from the application's already-running preview encoder.
#[derive(Clone)]
pub struct Frame {
    pub session: u64,
    pub sequence: u64,
    pub captured_at_unix_ms: u64,
    pub exposure_us: Option<i64>,
    pub gain: Option<i64>,
    pub mode: String,
    pub jpeg: Arc<[u8]>,
}
pub type FrameSource = Arc<dyn Fn() -> Option<Frame> + Send + Sync>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    pub hub_origin: String,
    pub enabled: bool,
    pub snapshots: bool,
    pub scene_changes: bool,
    pub day_night: bool,
    pub scene_threshold_percent: u8,
    pub interval_minutes: u16,
    pub telescope_events: bool,
    pub chat_configuration: bool,
    pub burst_count: u8,
    pub spacing_seconds: u16,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            hub_origin: String::new(),
            enabled: false,
            snapshots: false,
            scene_changes: false,
            day_night: false,
            scene_threshold_percent: 20,
            interval_minutes: 0,
            telescope_events: false,
            chat_configuration: false,
            burst_count: 1,
            spacing_seconds: 60,
        }
    }
}
impl Preferences {
    pub(crate) fn validate(&mut self, policy: TransportPolicy) -> Result<()> {
        if self.interval_minutes > 1440
            || !(1..=3).contains(&self.burst_count)
            || !(60..=600).contains(&self.spacing_seconds)
        {
            bail!("Use 0–1440 minutes, 1–3 images, and 60–600 seconds between images");
        }
        if !self.hub_origin.is_empty() {
            self.hub_origin = HubOrigin::parse(&self.hub_origin, policy)?.canonical();
        }
        if self.enabled && self.hub_origin.is_empty() {
            bail!("Set an HTTPS Hub origin first");
        }
        if !(5..=80).contains(&self.scene_threshold_percent) {
            bail!("Scene threshold must be between 5 and 80 percent");
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    pub installation_id: String,
    pub revision: u64,
    pub device_id: Option<i64>,
    pub preferences: Preferences,
    #[serde(default)]
    pub remote_rules: Option<crate::triggers::TriggerRules>,
}

impl Settings {
    pub fn rules(&self) -> crate::triggers::TriggerRules {
        self.remote_rules
            .as_ref()
            .filter(|r| self.preferences.chat_configuration && r.allowed_by(&self.preferences))
            .cloned()
            .unwrap_or_else(|| crate::triggers::TriggerRules::local(&self.preferences))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub revision: u64,
    pub installation_id: String,
    pub device_id: Option<i64>,
    pub preferences: Preferences,
    pub connection: String,
    pub last_delivery_unix_ms: Option<u64>,
    pub active_triggers: crate::triggers::TriggerRules,
}

pub(crate) struct Shared {
    pub settings: RwLock<Settings>,
    pub connection: Mutex<String>,
    pub last_delivery: Mutex<Option<u64>>,
    pub changes: watch::Sender<u64>,
    pub stopped: AtomicBool,
    pub vault: Arc<dyn CredentialStore>,
    pub source: FrameSource,
    pub policy: TransportPolicy,
    path: PathBuf,
    mutation: Mutex<()>,
    pairing: AtomicBool,
}

impl Shared {
    /// Called only by the transport task. Local mutations still cancel that
    /// task via `changes`; remote updates cannot alter consent or identity.
    pub fn configure_triggers(
        &self,
        rules: crate::triggers::TriggerRules,
        epoch: u64,
    ) -> Result<()> {
        let _guard = self.mutation.lock().unwrap();
        let mut settings = self.settings.read().unwrap().clone();
        if self.stopped.load(Ordering::Acquire)
            || *self.changes.borrow() != epoch
            || !settings.preferences.enabled
            || !settings.preferences.chat_configuration
            || !rules.allowed_by(&settings.preferences)
        {
            bail!("Trigger configuration exceeds local permissions");
        }
        settings.remote_rules = Some(rules);
        settings.revision = settings
            .revision
            .checked_add(1)
            .context("Sharing revision exhausted")?;
        persist(&self.path, &settings)?;
        *self.settings.write().unwrap() = settings;
        Ok(())
    }
}

#[derive(Clone)]
pub struct SharingClient(pub(crate) Arc<Shared>);

pub struct SharingService {
    client: SharingClient,
    thread: Option<JoinHandle<()>>,
    _lease: File,
}

impl SharingService {
    pub fn start(config_path: &Path, source: FrameSource) -> Result<Self> {
        Self::start_with_store(
            config_path,
            source,
            Arc::new(OsCredentialStore),
            TransportPolicy::HttpsOnly,
        )
    }

    /// Dependency injection for local mock-Hub tests and future OS stores.
    pub fn start_with_store(
        config_path: &Path,
        source: FrameSource,
        vault: Arc<dyn CredentialStore>,
        policy: TransportPolicy,
    ) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let path = std::path::absolute(config_path)?.with_extension("chatstronomy.json");
        std::fs::create_dir_all(
            path.parent()
                .context("Sharing settings need a parent directory")?,
        )?;
        let lease = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))?;
        fs4::FileExt::try_lock(&lease)
            .map_err(|_| anyhow::anyhow!("Another sharing worker owns this installation"))?;
        let settings = if path.exists() {
            if std::fs::metadata(&path)?.len() > 16 * 1024 {
                bail!("Sharing settings are too large");
            }
            let mut settings: Settings = serde_json::from_slice(&std::fs::read(&path)?)
                .map_err(|_| anyhow::anyhow!("Invalid sharing settings; sharing is disabled"))?;
            settings.preferences.validate(policy)?;
            if settings.remote_rules.as_ref().is_some_and(|rules| {
                !settings.preferences.chat_configuration || !rules.allowed_by(&settings.preferences)
            }) {
                bail!("Saved trigger rules exceed local permissions; sharing is disabled");
            }
            if !crate::protocol::valid_uuid(&settings.installation_id)
                || settings.device_id.is_some_and(|id| id <= 0)
            {
                bail!("Invalid sharing installation identity");
            }
            settings
        } else {
            let settings = Settings {
                installation_id: random_uuid()?,
                revision: 1,
                device_id: None,
                preferences: Preferences::default(),
                remote_rules: None,
            };
            persist(&path, &settings)?;
            settings
        };
        let (changes, _) = watch::channel(0);
        let shared = Arc::new(Shared {
            settings: RwLock::new(settings),
            connection: Mutex::new("Disabled".into()),
            last_delivery: Mutex::new(None),
            changes,
            stopped: AtomicBool::new(false),
            vault,
            source,
            policy,
            path,
            mutation: Mutex::new(()),
            pairing: AtomicBool::new(false),
        });
        let client = SharingClient(shared.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let thread = std::thread::Builder::new()
            .name("autopiercam-chatstronomy".into())
            .spawn(move || {
                runtime.block_on(crate::transport::run(shared));
            })?;
        Ok(Self {
            client,
            thread: Some(thread),
            _lease: lease,
        })
    }
    pub fn client(&self) -> SharingClient {
        self.client.clone()
    }
}

impl Drop for SharingService {
    fn drop(&mut self) {
        self.client.0.stopped.store(true, Ordering::Release);
        self.client.invalidate();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl SharingClient {
    pub fn status(&self) -> Status {
        let settings = self.0.settings.read().unwrap().clone();
        Status {
            active_triggers: settings.rules(),
            revision: settings.revision,
            installation_id: settings.installation_id,
            device_id: settings.device_id,
            preferences: settings.preferences,
            connection: self.0.connection.lock().unwrap().clone(),
            last_delivery_unix_ms: *self.0.last_delivery.lock().unwrap(),
        }
    }
    /// Cancels the current connection and its immutable image outbox.
    pub fn invalidate(&self) {
        self.0
            .changes
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    pub fn update(&self, expected_revision: u64, mut preferences: Preferences) -> Result<Status> {
        preferences.validate(self.0.policy)?;
        let _guard = self.0.mutation.lock().unwrap();
        let mut settings = self.0.settings.read().unwrap().clone();
        if settings.revision != expected_revision {
            bail!("Sharing settings changed; refresh before saving");
        }
        if settings.device_id.is_some() && settings.preferences.hub_origin != preferences.hub_origin
        {
            bail!("Forget the current pairing before changing Hub");
        }
        settings.preferences = preferences;
        settings.remote_rules = None;
        settings.revision = settings
            .revision
            .checked_add(1)
            .context("Sharing revision exhausted")?;
        self.commit(settings)?;
        Ok(self.status())
    }

    fn commit(&self, settings: Settings) -> Result<()> {
        // Cancel before disk I/O; a failed save must never leave sharing active.
        {
            let mut current = self.0.settings.write().unwrap();
            if current.device_id != settings.device_id
                || current.preferences.hub_origin != settings.preferences.hub_origin
            {
                *self.0.last_delivery.lock().unwrap() = None;
            }
            current.preferences.enabled = false;
        }
        *self.0.connection.lock().unwrap() = "Disabled".into();
        self.invalidate();
        persist(&self.0.path, &settings)?;
        *self.0.settings.write().unwrap() = settings;
        self.invalidate();
        Ok(())
    }

    /// Pairing never enables sharing, and is never automatically retried.
    pub fn pair(&self, expected_revision: u64, token: Secret) -> Result<Status> {
        if self.0.pairing.swap(true, Ordering::AcqRel) {
            bail!("Pairing is already in progress");
        }
        struct Reset<'a>(&'a AtomicBool);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _reset = Reset(&self.0.pairing);
        if token.expose_for_transport().len() > 512
            || !token.expose_for_transport().starts_with("csdp_")
        {
            bail!("Enter a valid device pairing code");
        }
        let mut settings = {
            let _guard = self.0.mutation.lock().unwrap();
            let mut settings = self.0.settings.read().unwrap().clone();
            if settings.revision != expected_revision {
                bail!("Sharing settings changed; refresh before pairing");
            }
            if settings.preferences.hub_origin.is_empty() {
                bail!("Save the Hub origin first");
            }
            settings.preferences.enabled = false;
            settings.preferences.snapshots = false;
            settings.preferences.scene_changes = false;
            settings.preferences.day_night = false;
            settings.preferences.interval_minutes = 0;
            settings.preferences.telescope_events = false;
            settings.preferences.chat_configuration = false;
            settings.remote_rules = None;
            settings.revision += 1;
            self.commit(settings.clone())?;
            settings
        };
        let origin = HubOrigin::parse(&settings.preferences.hub_origin, self.0.policy)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let response = runtime.block_on(crate::transport::pair(
            &origin,
            &settings.installation_id,
            &token,
        ))?;
        let _guard = self.0.mutation.lock().unwrap();
        if self.0.stopped.load(Ordering::Acquire)
            || self.0.settings.read().unwrap().revision != settings.revision
        {
            bail!("Pairing cancelled by a local change; issue a new Hub code");
        }
        self.0.vault.write(
            &vault::target(&origin.canonical(), &settings.installation_id),
            &response.credential,
        )?;
        settings.device_id = Some(response.device_id);
        settings.revision += 1;
        self.commit(settings)?;
        Ok(self.status())
    }

    /// Local stop/forget. Revocation of the server credential is performed in
    /// the Hub (its device protocol intentionally provides no revoke RPC).
    pub fn forget(&self, expected_revision: u64) -> Result<Status> {
        let _guard = self.0.mutation.lock().unwrap();
        let mut settings = self.0.settings.read().unwrap().clone();
        if settings.revision != expected_revision {
            bail!("Sharing settings changed; refresh before forgetting");
        }
        let target = vault::target(&settings.preferences.hub_origin, &settings.installation_id);
        settings.preferences.enabled = false;
        settings.preferences.snapshots = false;
        settings.preferences.scene_changes = false;
        settings.preferences.day_night = false;
        settings.preferences.interval_minutes = 0;
        settings.preferences.telescope_events = false;
        settings.preferences.chat_configuration = false;
        settings.remote_rules = None;
        settings.device_id = None;
        settings.revision += 1;
        self.commit(settings)?;
        self.0.vault.delete(&target)?;
        Ok(self.status())
    }
}

fn persist(path: &Path, settings: &Settings) -> Result<()> {
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().context("Sharing directory missing")?)?;
    file.write_all(&serde_json::to_vec_pretty(settings)?)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .map_err(|_| anyhow::anyhow!("Could not save sharing settings"))?;
    Ok(())
}

pub(crate) fn random_uuid() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Could not create random identifier"))?;
    bytes[6] = (bytes[6] & 15) | 0x40;
    bytes[8] = (bytes[8] & 63) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}
