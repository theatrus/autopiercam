//! Incremental local datasets. Originals are copied, not modified. Imports never
//! infer labels, overwrite annotations, or assign random frame-level splits.
use crate::{MAX_IMAGE_BYTES, model::Task, preprocess, read_bounded, sha256};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const MAX_MANIFEST_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Labels {
    pub roof: Option<String>,
    pub sky: Option<String>,
    pub quality: Option<String>,
}
impl Labels {
    fn validate(&self) -> Result<()> {
        for (task, label) in [
            (Task::Roof, &self.roof),
            (Task::Sky, &self.sky),
            (Task::Quality, &self.quality),
        ] {
            if let Some(label) = label {
                ensure!(
                    task.labels().contains(&label.as_str())
                        || label == "uncertain"
                        || (task == Task::Sky && label == "not_visible"),
                    "Invalid {task:?} label: {label}"
                );
            }
        }
        if self.roof.as_deref() == Some("closed") {
            ensure!(
                matches!(
                    self.sky.as_deref(),
                    None | Some("not_visible" | "uncertain")
                ),
                "A closed roof cannot have a visible-sky label"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    pub image: String,
    pub source_file: String,
    pub captured_unix_ms: Option<u64>,
    pub width: u32,
    pub height: u32,
    pub site: String,
    pub camera: String,
    /// A whole observing session/night, never a per-frame split assignment.
    pub group: String,
    pub labels: Labels,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dataset {
    pub schema_version: u32,
    pub samples: BTreeMap<String, Sample>,
}
impl Default for Dataset {
    fn default() -> Self {
        Self {
            schema_version: 1,
            samples: BTreeMap::new(),
        }
    }
}
impl Dataset {
    fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "Unsupported dataset schema");
        for (id, sample) in &self.samples {
            ensure!(
                id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid sample digest"
            );
            ensure!(
                [format!("images/{id}.jpg"), format!("images/{id}.png")].contains(&sample.image),
                "Invalid dataset image path"
            );
            for value in [&sample.site, &sample.camera, &sample.group] {
                validate_name(value)?;
            }
            sample.labels.validate()?;
        }
        Ok(())
    }
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty() && name.len() <= 200 && !name.chars().any(char::is_control),
        "Site, camera and group must be nonempty plain text (max 200 bytes)"
    );
    Ok(())
}

fn atomic_json(path: &Path, data: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(data)?;
    ensure!(
        (bytes.len() as u64) < MAX_MANIFEST_BYTES,
        "Dataset metadata exceeds size limit"
    );
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().context("Missing parent")?)?;
    temp.write_all(&bytes)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}

struct Store {
    root: PathBuf,
    _lock: File,
}
impl Store {
    fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join(".lock"))?;
        fs4::FileExt::try_lock(&lock).context("Dataset is in use by another process")?;
        fs::create_dir_all(root.join("images"))?;
        // Also protect datasets created outside the repository's ignored folder.
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join(".gitignore"))
        {
            Ok(mut file) => file.write_all(b"*\n")?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
        Ok(Self { root, _lock: lock })
    }
    fn load(&self) -> Result<Dataset> {
        let path = self.root.join("manifest.json");
        let data = if path.exists() {
            serde_json::from_slice(&read_bounded(&path, MAX_MANIFEST_BYTES)?)?
        } else {
            Dataset::default()
        };
        Dataset::validate(&data)?;
        Ok(data)
    }
    fn save(&self, data: &Dataset) -> Result<()> {
        data.validate()?;
        atomic_json(&self.root.join("manifest.json"), data)
    }
}

pub struct ImportOptions {
    pub source: PathBuf,
    pub output: PathBuf,
    pub site: String,
    pub camera: String,
    pub group: String,
    pub interval_seconds: u64,
    pub min_age_seconds: u64,
    pub limit: usize,
}

#[derive(Default, Debug, Serialize)]
pub struct ImportReport {
    pub scanned: usize,
    pub imported: usize,
    pub duplicate: usize,
    pub sampled_out: usize,
    pub pending: usize,
    pub invalid: usize,
    pub total: usize,
    pub warnings: Vec<String>,
}

/// Parse only AutoPierCam's generated still filename. Never guess exposure,
/// gain, roof state, or weather from a filename or timestamp.
fn capture_time(path: &Path) -> Option<u64> {
    let stem = path.file_stem()?.to_str()?;
    let mut parts = stem.split('-');
    if parts.next()? != "frame" {
        return None;
    }
    let seconds = parts.next()?.parse::<u64>().ok()?;
    let millis = parts.next()?.parse::<u64>().ok()?;
    let session = parts.next()?;
    if millis >= 1000
        || session.len() != 32
        || !session.bytes().all(|b| b.is_ascii_hexdigit())
        || parts.next()?.parse::<u64>().is_err()
        || parts.next().is_some()
    {
        return None;
    }
    seconds.checked_mul(1000)?.checked_add(millis)
}

// Resolve an output that may not exist yet before creating anything. In
// particular, an accidental dataset path inside the source must not modify it.
fn prospective_directory(path: &Path) -> Result<PathBuf> {
    ensure!(
        !path
            .components()
            .any(|c| c == std::path::Component::ParentDir),
        "Use an output path without parent traversals"
    );
    let mut cursor = std::path::absolute(path)?;
    let mut missing = Vec::new();
    while !cursor.exists() {
        missing.push(
            cursor
                .file_name()
                .context("Output has no existing ancestor")?
                .to_os_string(),
        );
        ensure!(cursor.pop(), "Output has no existing ancestor");
    }
    let mut resolved = cursor.canonicalize()?;
    for part in missing.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

fn collect(directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect(&entry.path(), output)?;
        } else if kind.is_file()
            && matches!(
                entry
                    .path()
                    .extension()
                    .and_then(|v| v.to_str())
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("jpg" | "jpeg" | "png")
            )
        {
            ensure!(
                output.len() < 100_000,
                "Too many source files; import a smaller directory"
            );
            output.push(entry.path());
        }
    }
    Ok(())
}

fn complete(bytes: &[u8]) -> bool {
    // JPEG decoders may tolerate a truncated scan: insist on the final marker.
    if bytes.starts_with(&[0xff, 0xd8]) {
        bytes.ends_with(&[0xff, 0xd9])
    } else {
        bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.ends_with(b"\0\0\0\0IEND\xaeB`\x82")
    }
}

pub fn import(options: &ImportOptions) -> Result<ImportReport> {
    for value in [&options.site, &options.camera, &options.group] {
        validate_name(value)?;
    }
    ensure!(
        (1..=10_000).contains(&options.limit),
        "Import limit must be 1–10000"
    );
    let interval_ms = options
        .interval_seconds
        .checked_mul(1000)
        .context("Interval too large")?;
    let source = options.source.canonicalize()?;
    ensure!(source.is_dir(), "Source must be a directory");
    let output = prospective_directory(&options.output)?;
    ensure!(
        !output.starts_with(&source) && !source.starts_with(&output),
        "Source and dataset directories must be separate"
    );
    let store = Store::open(&output)?;
    let mut data = store.load()?;
    let mut files = Vec::new();
    collect(&source, &mut files)?;
    files.sort();
    let mut report = ImportReport::default();
    let mut buckets = BTreeSet::new();
    for sample in data.samples.values().filter(|s| {
        s.site == options.site && s.camera == options.camera && s.group == options.group
    }) {
        if let Some(bucket) = sample
            .captured_unix_ms
            .and_then(|time| time.checked_div(interval_ms))
        {
            buckets.insert(bucket);
        }
    }
    for path in files {
        if report.imported >= options.limit {
            break;
        }
        report.scanned += 1;
        let time = capture_time(&path);
        if let Some(bucket) = time.and_then(|v| v.checked_div(interval_ms))
            && buckets.contains(&bucket)
        {
            report.sampled_out += 1;
            continue;
        }
        let attempt = (|| -> Result<Option<(Vec<u8>, u32, u32)>> {
            let before = path.metadata()?;
            if SystemTime::now()
                .duration_since(before.modified()?)
                .unwrap_or_default()
                < Duration::from_secs(options.min_age_seconds)
            {
                return Ok(None);
            }
            let bytes = read_bounded(&path, MAX_IMAGE_BYTES)?;
            let after = path.metadata()?;
            if before.len() != after.len()
                || before.modified()? != after.modified()?
                || !complete(&bytes)
            {
                return Ok(None);
            }
            let decoded = preprocess::decode(&bytes)?;
            Ok(Some((bytes, decoded.width(), decoded.height())))
        })();
        let (bytes, width, height) = match attempt {
            Ok(Some(image)) => image,
            Ok(None) => {
                report.pending += 1;
                continue;
            }
            Err(error) => {
                report.invalid += 1;
                if report.warnings.len() < 20 {
                    report.warnings.push(format!("{}: {error}", path.display()));
                }
                continue;
            }
        };
        let id = sha256(&bytes);
        if data.samples.contains_key(&id) {
            report.duplicate += 1;
            continue;
        }
        let extension = if bytes.starts_with(&[0xff, 0xd8]) {
            "jpg"
        } else {
            "png"
        };
        let image = format!("images/{id}.{extension}");
        let target = store.root.join(&image);
        if target.exists() {
            ensure!(
                sha256(&read_bounded(&target, MAX_IMAGE_BYTES)?) == id,
                "Existing dataset image is corrupt: {id}"
            );
        } else {
            let mut temp = tempfile::NamedTempFile::new_in(store.root.join("images"))?;
            temp.write_all(&bytes)?;
            temp.as_file().sync_all()?;
            temp.persist_noclobber(&target)?;
        }
        data.samples.insert(
            id,
            Sample {
                image,
                source_file: path.to_string_lossy().into_owned(),
                captured_unix_ms: time,
                width,
                height,
                site: options.site.clone(),
                camera: options.camera.clone(),
                group: options.group.clone(),
                labels: Labels::default(),
            },
        );
        if let Some(bucket) = time.and_then(|v| v.checked_div(interval_ms)) {
            buckets.insert(bucket);
        }
        report.imported += 1;
    }
    report.total = data.samples.len();
    store.save(&data)?;
    Ok(report)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Annotations {
    pub schema_version: u32,
    pub edits: BTreeMap<String, LabelEdit>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelEdit {
    pub before: Labels,
    pub after: Labels,
}

pub fn apply_labels(root: &Path, file: &Path) -> Result<usize> {
    ensure!(root.join("manifest.json").is_file(), "Import images first");
    let store = Store::open(root)?;
    let mut data = store.load()?;
    let update: Annotations = serde_json::from_slice(&read_bounded(file, MAX_MANIFEST_BYTES)?)?;
    ensure!(update.schema_version == 1, "Unsupported annotation schema");
    // Validate the entire batch before writing anything.
    for (id, edit) in &update.edits {
        let sample = data
            .samples
            .get(id)
            .with_context(|| format!("Unknown sample: {id}"))?;
        edit.before.validate()?;
        edit.after.validate()?;
        ensure!(
            sample.labels == edit.before || sample.labels == edit.after,
            "Newer labels conflict for {id}; regenerate review and reconcile this image"
        );
    }
    for (id, edit) in &update.edits {
        data.samples.get_mut(id).unwrap().labels = edit.after.clone();
    }
    store.save(&data)?;
    Ok(update.edits.len())
}

pub fn review(root: &Path) -> Result<PathBuf> {
    ensure!(root.join("manifest.json").is_file(), "Import images first");
    let store = Store::open(root)?;
    let data = store.load()?;
    ensure!(!data.samples.is_empty(), "Import images first");
    let payload = serde_json::json!({"samples": data.samples});
    // Escape '<' even inside JSON strings so source filenames cannot end script.
    let json = serde_json::to_string(&payload)?.replace('<', "\\u003c");
    let html = include_str!("review.html").replace("/*DATASET_JSON*/", &json);
    let path = store.root.join("review.html");
    let mut temp = tempfile::NamedTempFile::new_in(&store.root)?;
    temp.write_all(html.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(&path)?;
    Ok(path)
}

/// Validate copies and report coverage without guessing class labels.
pub fn check(root: &Path) -> Result<serde_json::Value> {
    ensure!(root.join("manifest.json").is_file(), "Import images first");
    let store = Store::open(root)?;
    let data = store.load()?;
    let mut groups = BTreeMap::<String, usize>::new();
    let mut labeled = 0;
    for (id, sample) in &data.samples {
        ensure!(
            sha256(&read_bounded(
                &store.root.join(&sample.image),
                MAX_IMAGE_BYTES
            )?) == *id,
            "Image checksum mismatch: {id}"
        );
        *groups.entry(sample.group.clone()).or_default() += 1;
        if sample.labels != Labels::default() {
            labeled += 1;
        }
    }
    Ok(
        serde_json::json!({"samples": data.samples.len(), "annotated_samples": labeled, "groups": groups,
        "training_ready": false, "note": "Labels, class coverage, and disjoint observing groups need review before training."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, ImportOptions) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("source")).unwrap();
        let options = ImportOptions {
            source: dir.path().join("source"),
            output: dir.path().join("dataset"),
            site: "test".into(),
            camera: "camera".into(),
            group: "night-1".into(),
            interval_seconds: 0,
            min_age_seconds: 0,
            limit: 100,
        };
        (dir, options)
    }
    fn add(options: &ImportOptions, filename: &str, color: u8) {
        fs::write(
            options.source.join(filename),
            crate::preprocess::png(32, 24, [color; 3]),
        )
        .unwrap();
    }
    #[test]
    fn incremental_import_deduplicates_and_preserves_labels() {
        let (_dir, options) = setup();
        add(&options, "one.png", 80);
        assert_eq!(import(&options).unwrap().imported, 1);
        let store = Store::open(&options.output).unwrap();
        let mut data = store.load().unwrap();
        data.samples.values_mut().next().unwrap().labels.roof = Some("open".into());
        store.save(&data).unwrap();
        drop(store);
        add(&options, "duplicate.png", 80);
        add(&options, "two.png", 90);
        let report = import(&options).unwrap();
        assert_eq!((report.imported, report.duplicate, report.total), (1, 2, 2));
        assert_eq!(check(&options.output).unwrap()["annotated_samples"], 1);
    }
    #[test]
    fn incomplete_and_recent_files_are_retried() {
        let (_dir, mut options) = setup();
        fs::write(options.source.join("copying.png"), b"partial").unwrap();
        assert_eq!(import(&options).unwrap().pending, 1);
        add(&options, "copying.png", 80);
        options.min_age_seconds = 3600;
        assert_eq!(import(&options).unwrap().pending, 1);
        options.min_age_seconds = 0;
        assert_eq!(import(&options).unwrap().imported, 1);
    }
    #[test]
    fn sampling_uses_capture_time_and_stays_incremental() {
        let (_dir, mut options) = setup();
        options.interval_seconds = 300;
        for (seconds, color) in [(1800, 80), (1810, 81), (2100, 90)] {
            add(
                &options,
                &format!("frame-{seconds}-123-0123456789abcdef0123456789abcdef-000001.png"),
                color,
            );
        }
        let report = import(&options).unwrap();
        assert_eq!((report.imported, report.sampled_out), (2, 1));
        assert_eq!(import(&options).unwrap().imported, 0);
        assert_eq!(
            capture_time(Path::new(
                "frame-1800-123-0123456789abcdef0123456789abcdef-000001.jpg"
            )),
            Some(1_800_123)
        );
        assert_eq!(capture_time(Path::new("photo.png")), None);
    }
    #[test]
    fn annotations_merge_across_imports_but_reject_conflicting_edits() {
        let (_dir, options) = setup();
        add(&options, "one.png", 80);
        import(&options).unwrap();
        let store = Store::open(&options.output).unwrap();
        let data = store.load().unwrap();
        let id = data.samples.keys().next().unwrap().clone();
        drop(store);
        let path = options.output.join("labels.json");
        let mut update = Annotations {
            schema_version: 1,
            edits: BTreeMap::from([(
                id.clone(),
                LabelEdit {
                    before: Labels::default(),
                    after: Labels {
                        roof: Some("closed".into()),
                        sky: Some("clear".into()),
                        quality: None,
                    },
                },
            )]),
        };
        atomic_json(&path, &update).unwrap();
        assert!(apply_labels(&options.output, &path).is_err());
        update.edits.values_mut().next().unwrap().after.sky = Some("not_visible".into());
        add(&options, "two.png", 90);
        import(&options).unwrap();
        atomic_json(&path, &update).unwrap();
        assert_eq!(apply_labels(&options.output, &path).unwrap(), 1);
        assert_eq!(apply_labels(&options.output, &path).unwrap(), 1); // idempotent replay
        let store = Store::open(&options.output).unwrap();
        let mut data = store.load().unwrap();
        data.samples.get_mut(&id).unwrap().labels.quality = Some("usable".into());
        store.save(&data).unwrap();
        drop(store);
        assert!(apply_labels(&options.output, &path).is_err());
    }
    #[test]
    fn corruption_traversal_and_nested_source_are_rejected() {
        let (_dir, mut options) = setup();
        add(&options, "one.png", 80);
        import(&options).unwrap();
        let store = Store::open(&options.output).unwrap();
        let mut data = store.load().unwrap();
        let sample = data.samples.values_mut().next().unwrap();
        fs::write(store.root.join(&sample.image), b"corrupt").unwrap();
        sample.image = "../outside.jpg".into();
        assert!(store.save(&data).is_err());
        drop(store);
        assert!(check(&options.output).is_err());
        options.output = options.source.join("nested");
        assert!(import(&options).is_err());
        assert!(
            !options.output.exists(),
            "rejection must not write into the source"
        );
    }
    #[test]
    fn review_is_local_and_escapes_embedded_data() {
        let (_dir, mut options) = setup();
        options.site = "</script><script>alert(1)</script>".into();
        add(&options, "one.png", 80);
        import(&options).unwrap();
        let html = fs::read_to_string(review(&options.output).unwrap()).unwrap();
        assert!(!html.contains("</script><script>alert"));
        assert!(html.contains("\\u003c/script>"));
        assert!(!html.contains("https://"));
    }
    #[test]
    fn dataset_lock_excludes_concurrent_writers() {
        let (_dir, options) = setup();
        let store = Store::open(&options.output).unwrap();
        assert!(Store::open(&options.output).is_err());
        drop(store);
        assert!(Store::open(&options.output).is_ok());
    }
}
