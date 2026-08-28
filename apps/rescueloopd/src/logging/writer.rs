use anyhow::{Context, Result};
use chrono::{Local, NaiveDate};
use flate2::{Compression, write::GzEncoder};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tracing_subscriber::fmt::MakeWriter;

use super::{export::ExportSink, fallback};

const LOG_PREFIX: &str = "rescueloop-";

pub struct WriterConfig {
    pub directory: PathBuf,
    pub max_file_bytes: u64,
    pub retention_days: usize,
    pub compress_rotated: bool,
    pub run_id: String,
    pub export: Option<ExportSink>,
}

#[derive(Clone)]
pub struct LogHealth {
    write_errors: Arc<AtomicU64>,
    export_drops: Arc<AtomicU64>,
}

impl LogHealth {
    pub fn write_errors(&self) -> u64 {
        self.write_errors.load(Ordering::Relaxed)
    }

    pub fn export_drops(&self) -> u64 {
        self.export_drops.load(Ordering::Relaxed)
    }
}

pub struct RollingWriter {
    state: Mutex<State>,
    health: LogHealth,
}

struct State {
    config: WriterConfig,
    file: Option<File>,
    path: PathBuf,
    date: NaiveDate,
    bytes_written: u64,
    sequence: u32,
    _run_lock: File,
    record_sequence: u64,
    previous_hash: String,
    started_at: Instant,
}

impl RollingWriter {
    pub fn new(config: WriterConfig) -> Result<Self> {
        uuid::Uuid::parse_str(&config.run_id).context("log run_id must be a UUID")?;
        fs::create_dir_all(&config.directory)?;
        let maintenance_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(config.directory.join(".rescueloop-log-maintenance"))?;
        lock_exclusive_wait(&maintenance_lock)?;
        maintain_inactive(&config.directory, config.retention_days)?;
        FileExt::unlock(&maintenance_lock)?;
        let lock_path = run_lock_path(&config.directory, &config.run_id);
        let run_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        run_lock
            .try_lock_exclusive()
            .with_context(|| format!("log run is already active: {}", config.run_id))?;
        let date = Local::now().date_naive();
        let sequence = 0;
        let path = log_path(&config.directory, &config.run_id, date, sequence);
        let file = open(&path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self {
            state: Mutex::new(State {
                config,
                file: Some(file),
                path,
                date,
                bytes_written,
                sequence,
                _run_lock: run_lock,
                record_sequence: 0,
                previous_hash: String::new(),
                started_at: Instant::now(),
            }),
            health: LogHealth {
                write_errors: Arc::new(AtomicU64::new(0)),
                export_drops: Arc::new(AtomicU64::new(0)),
            },
        })
    }

    pub fn health(&self) -> LogHealth {
        self.health.clone()
    }
}

impl<'a> MakeWriter<'a> for RollingWriter {
    type Writer = EventWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        EventWriter {
            state: Some(self.state.lock().unwrap_or_else(|error| error.into_inner())),
            health: &self.health,
            buffer: Vec::new(),
            committed: false,
        }
    }
}

pub struct EventWriter<'a> {
    state: Option<MutexGuard<'a, State>>,
    health: &'a LogHealth,
    buffer: Vec<u8>,
    committed: bool,
}

impl Write for EventWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.commit()?;
        self.state.as_mut().map_or(Ok(()), |state| state.flush())
    }
}

impl Drop for EventWriter<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.commit() {
            self.health.write_errors.fetch_add(1, Ordering::Relaxed);
            fallback::emergency(&format!("RescueLoop log write failed: {error}"));
        }
    }
}

impl EventWriter<'_> {
    fn commit(&mut self) -> io::Result<()> {
        if self.committed || self.buffer.is_empty() {
            return Ok(());
        }
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| io::Error::other("log writer state is unavailable"))?;
        let (encoded, hash) = enrich_and_redact(
            &self.buffer,
            &state.config.run_id,
            state.record_sequence,
            &state.previous_hash,
            state.started_at.elapsed().as_nanos(),
        )?;
        state.write(&encoded, self.health)?;
        state.previous_hash = hash;
        state.record_sequence = state.record_sequence.saturating_add(1);
        self.committed = true;
        Ok(())
    }
}

impl State {
    fn write(&mut self, buffer: &[u8], health: &LogHealth) -> io::Result<usize> {
        let today = Local::now().date_naive();
        if today != self.date
            || (self.bytes_written > 0
                && self.bytes_written.saturating_add(buffer.len() as u64)
                    > self.config.max_file_bytes)
        {
            self.rotate(today)?;
        }
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file is unavailable"))?
            .write_all(buffer)?;
        self.bytes_written = self.bytes_written.saturating_add(buffer.len() as u64);
        if let Some(export) = &self.config.export
            && let Err(error) = export.enqueue(buffer)
        {
            health.export_drops.fetch_add(1, Ordering::Relaxed);
            fallback::emergency(&format!("RescueLoop export spool write failed: {error}"));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }

    fn rotate(&mut self, date: NaiveDate) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }
        if self.config.compress_rotated {
            compress(&self.path)?;
        }
        self.sequence = if date == self.date {
            self.sequence.saturating_add(1)
        } else {
            0
        };
        self.date = date;
        self.path = log_path(
            &self.config.directory,
            &self.config.run_id,
            date,
            self.sequence,
        );
        let file = open(&self.path)?;
        self.bytes_written = file.metadata()?.len();
        self.file = Some(file);
        maintain_inactive(&self.config.directory, self.config.retention_days)
            .map_err(io::Error::other)?;
        Ok(())
    }
}

fn open(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn lock_exclusive_wait(file: &File) -> io::Result<()> {
    const RETRY_DELAY: Duration = Duration::from_millis(10);
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if lock_is_contended(&error) => std::thread::sleep(RETRY_DELAY),
            Err(error) => return Err(error),
        }
    }
}

fn lock_is_contended(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33))
}

fn log_path(directory: &Path, run_id: &str, date: NaiveDate, sequence: u32) -> PathBuf {
    directory.join(format!(
        "{LOG_PREFIX}{}-{run_id}-{sequence:04}.jsonl",
        date.format("%Y-%m-%d"),
    ))
}

fn run_lock_path(directory: &Path, run_id: &str) -> PathBuf {
    directory.join(format!("{LOG_PREFIX}{run_id}.lock"))
}

fn run_id_from_log_path(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?.strip_prefix(LOG_PREFIX)?;
    let after_date = name.get(11..)?;
    let run_id = after_date.get(..36)?;
    uuid::Uuid::parse_str(run_id).ok().map(|_| run_id)
}

fn maintain_inactive(directory: &Path, retention_days: usize) -> Result<()> {
    prune_expired(directory, retention_days)?;
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_some_and(|value| value == "jsonl") && !is_active(&path)? {
            compress(&path)?;
        }
    }
    remove_inactive_locks(directory)?;
    Ok(())
}

fn remove_inactive_locks(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_none_or(|value| value != "lock") {
            continue;
        }
        let lock = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        match lock.try_lock_exclusive() {
            Ok(()) => {
                FileExt::unlock(&lock)?;
                drop(lock);
                if let Err(error) = fs::remove_file(&path)
                    && error.kind() != io::ErrorKind::NotFound
                {
                    return Err(error).with_context(|| {
                        format!("cannot remove inactive log lock: {}", path.display())
                    });
                }
            }
            Err(error) if lock_is_contended(&error) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn is_active(path: &Path) -> Result<bool> {
    let Some(run_id) = run_id_from_log_path(path) else {
        return Ok(false);
    };
    let lock_path = run_lock_path(path.parent().unwrap_or(Path::new(".")), run_id);
    if !lock_path.exists() {
        return Ok(false);
    }
    let lock = match OpenOptions::new().read(true).write(true).open(lock_path) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    match lock.try_lock_exclusive() {
        Ok(()) => {
            FileExt::unlock(&lock)?;
            Ok(false)
        }
        Err(error) if lock_is_contended(&error) => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn compress(path: &Path) -> io::Result<()> {
    let destination = PathBuf::from(format!("{}.gz", path.display()));
    let mut input = match File::open(path) {
        Ok(input) => input,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let output = File::create(&destination)?;
    let mut encoder = GzEncoder::new(output, Compression::fast());
    io::copy(&mut input, &mut encoder)?;
    encoder.finish()?.sync_all()?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn prune_expired(directory: &Path, retention_days: usize) -> Result<()> {
    let max_age = Duration::from_secs((retention_days as u64).saturating_mul(86_400));
    let now = SystemTime::now();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(LOG_PREFIX) || !entry.file_type()?.is_file() {
            continue;
        }
        if !matches!(
            entry.path().extension().and_then(|value| value.to_str()),
            Some("jsonl" | "gz")
        ) {
            continue;
        }
        if is_active(&entry.path())? {
            continue;
        }
        let modified = entry.metadata()?.modified()?;
        if now.duration_since(modified).is_ok_and(|age| age > max_age) {
            fs::remove_file(entry.path()).with_context(|| {
                format!("cannot remove expired log: {}", entry.path().display())
            })?;
        }
    }
    Ok(())
}

fn enrich_and_redact(
    buffer: &[u8],
    run_id: &str,
    sequence: u64,
    previous_hash: &str,
    monotonic_ns: u128,
) -> io::Result<(Vec<u8>, String)> {
    let mut record: serde_json::Value = serde_json::from_slice(buffer)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    redact(&mut record, None);
    let object = record
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "log record is not an object"))?;
    object.insert("schema_version".into(), 1.into());
    object.insert("run_id".into(), run_id.into());
    object.insert("sequence".into(), sequence.into());
    object.insert("previous_hash".into(), previous_hash.into());
    object.insert("monotonic_ns".into(), monotonic_ns.to_string().into());
    if let Some(fields) = object
        .get_mut("fields")
        .and_then(serde_json::Value::as_object_mut)
        && !fields.contains_key("event")
    {
        fields.insert("event".into(), "span.closed".into());
    }
    let correlation = object
        .get("fields")
        .and_then(serde_json::Value::as_object)
        .and_then(|fields| {
            fields
                .get("incident_id")
                .or_else(|| fields.get("transaction_id"))
        })
        .and_then(serde_json::Value::as_str)
        .unwrap_or(run_id)
        .to_string();
    object.insert("correlation_id".into(), correlation.into());
    let canonical = serde_json::to_vec(&record).map_err(io::Error::other)?;
    let hash = format!("{:x}", Sha256::digest(&canonical));
    record
        .as_object_mut()
        .expect("record was validated as an object")
        .insert("record_hash".into(), hash.clone().into());
    let mut encoded = serde_json::to_vec(&record).map_err(io::Error::other)?;
    encoded.push(b'\n');
    Ok((encoded, hash))
}

fn redact(value: &mut serde_json::Value, key: Option<&str>) {
    if key.is_some_and(is_sensitive_key) {
        *value = serde_json::Value::String("[REDACTED]".into());
        return;
    }
    match value {
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                redact(value, Some(key));
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact(value, key);
            }
        }
        serde_json::Value::String(text) => redact_home(text),
        _ => {}
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "token",
        "password",
        "secret",
        "authorization",
        "bearer",
        "arguments",
        "command_line",
        "raw_evidence",
        "file_content",
    ]
    .iter()
    .any(|sensitive| key.contains(sensitive))
        || matches!(key.as_str(), "path" | "directory" | "artifact")
        || key.ends_with("_path")
}

fn redact_home(text: &mut String) {
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if !home.is_empty() && text.contains(home.as_ref()) {
            *text = text.replace(home.as_ref(), "<HOME>");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use uuid::Uuid;

    fn temp_directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!("rescueloop-writer-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn config(directory: &Path, run_id: Uuid) -> WriterConfig {
        WriterConfig {
            directory: directory.to_path_buf(),
            max_file_bytes: 1,
            retention_days: 14,
            compress_rotated: true,
            run_id: run_id.to_string(),
            export: None,
        }
    }

    #[test]
    fn rotates_by_size_and_compresses_previous_file() {
        let directory = temp_directory();
        let writer = RollingWriter::new(config(&directory, Uuid::new_v4())).unwrap();
        writer
            .make_writer()
            .write_all(br#"{"fields":{"event":"first"}}"#)
            .unwrap();
        writer
            .make_writer()
            .write_all(br#"{"fields":{"event":"second"}}"#)
            .unwrap();

        let compressed = fs::read_dir(&directory)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|value| value.path()))
            .find(|path| path.extension().is_some_and(|value| value == "gz"))
            .unwrap();
        let mut decoded = String::new();
        flate2::read::GzDecoder::new(File::open(&compressed).unwrap())
            .read_to_string(&mut decoded)
            .unwrap();
        let record: serde_json::Value = serde_json::from_str(&decoded).unwrap();
        assert_eq!(record["fields"]["event"], "first");
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn records_write_health() {
        let health = LogHealth {
            write_errors: Arc::new(AtomicU64::new(2)),
            export_drops: Arc::new(AtomicU64::new(3)),
        };
        assert_eq!(health.write_errors(), 2);
        assert_eq!(health.export_drops(), 3);
    }

    #[test]
    fn adds_context_and_redacts_sensitive_fields() {
        let (encoded, hash) = enrich_and_redact(
            br#"{"fields":{"event":"test","token":"secret","incident_id":"incident-1"}}"#,
            "run-1",
            0,
            "",
            0,
        )
        .unwrap();
        let record: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(record["schema_version"], 1);
        assert_eq!(record["run_id"], "run-1");
        assert_eq!(record["correlation_id"], "incident-1");
        assert_eq!(record["fields"]["token"], "[REDACTED]");
        assert_eq!(record["record_hash"], hash);
    }

    #[test]
    fn does_not_compress_another_active_process_log() {
        let directory = temp_directory();
        let first = RollingWriter::new(config(&directory, Uuid::new_v4())).unwrap();
        first
            .make_writer()
            .write_all(br#"{"fields":{"event":"first"}}"#)
            .unwrap();

        let second = RollingWriter::new(config(&directory, Uuid::new_v4())).unwrap();
        second
            .make_writer()
            .write_all(br#"{"fields":{"event":"second"}}"#)
            .unwrap();

        let jsonl_count = fs::read_dir(&directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "jsonl")
            })
            .count();
        assert_eq!(jsonl_count, 2);
        drop((first, second));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_writers_preserve_every_record() {
        let directory = temp_directory();
        let handles = (0..4)
            .map(|worker| {
                let directory = directory.clone();
                std::thread::spawn(move || {
                    let mut config = config(&directory, Uuid::new_v4());
                    config.max_file_bytes = 1024 * 1024;
                    let writer = RollingWriter::new(config).unwrap();
                    for sequence in 0..100 {
                        writer
                            .make_writer()
                            .write_all(
                                format!(
                                    r#"{{"fields":{{"event":"stress","worker":{worker},"item":{sequence}}}}}"#
                                )
                                .as_bytes(),
                            )
                            .unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap();
        }
        maintain_inactive(&directory, 14).unwrap();

        let mut records = 0;
        for entry in fs::read_dir(&directory).unwrap().flatten() {
            if entry.path().extension().is_some_and(|value| value == "gz") {
                let mut decoded = String::new();
                flate2::read::GzDecoder::new(File::open(entry.path()).unwrap())
                    .read_to_string(&mut decoded)
                    .unwrap();
                records += decoded.lines().count();
            }
        }
        assert_eq!(records, 400);
        assert!(!fs::read_dir(&directory).unwrap().flatten().any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|value| value == "lock")
        }));
        fs::remove_dir_all(directory).unwrap();
    }
}
