use anyhow::{Context, Result};
use chrono::{Local, NaiveDate};
use flate2::{Compression, write::GzEncoder};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};
use tracing_subscriber::fmt::MakeWriter;

const LOG_PREFIX: &str = "rescueloop-";

pub struct WriterConfig {
    pub directory: PathBuf,
    pub max_file_bytes: u64,
    pub retention_days: usize,
    pub compress_rotated: bool,
    pub run_id: String,
}

#[derive(Clone)]
pub struct LogHealth {
    write_errors: Arc<AtomicU64>,
}

impl LogHealth {
    pub fn write_errors(&self) -> u64 {
        self.write_errors.load(Ordering::Relaxed)
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
}

impl RollingWriter {
    pub fn new(config: WriterConfig) -> Result<Self> {
        fs::create_dir_all(&config.directory)?;
        prune_expired(&config.directory, config.retention_days)?;
        compress_inactive(&config.directory)?;
        let date = Local::now().date_naive();
        let sequence = next_sequence(&config.directory, date);
        let path = log_path(&config.directory, date, sequence);
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
            }),
            health: LogHealth {
                write_errors: Arc::new(AtomicU64::new(0)),
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
            let _ = writeln!(io::stderr(), "RescueLoop log write failed: {error}");
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
        let encoded = enrich_and_redact(&self.buffer, &state.config.run_id)?;
        state.write(&encoded)?;
        self.committed = true;
        Ok(())
    }
}

impl State {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let today = Local::now().date_naive();
        if today != self.date
            || (self.bytes_written > 0
                && self.bytes_written.saturating_add(buffer.len() as u64)
                    > self.config.max_file_bytes)
        {
            self.rotate(today)?;
        }
        let written = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file is unavailable"))?
            .write(buffer)?;
        self.bytes_written = self.bytes_written.saturating_add(written as u64);
        Ok(written)
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
            next_sequence(&self.config.directory, date)
        };
        self.date = date;
        self.path = log_path(&self.config.directory, date, self.sequence);
        let file = open(&self.path)?;
        self.bytes_written = file.metadata()?.len();
        self.file = Some(file);
        prune_expired(&self.config.directory, self.config.retention_days)
            .map_err(io::Error::other)?;
        Ok(())
    }
}

fn open(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn log_path(directory: &Path, date: NaiveDate, sequence: u32) -> PathBuf {
    directory.join(format!(
        "{LOG_PREFIX}{}-{sequence:04}.jsonl",
        date.format("%Y-%m-%d")
    ))
}

fn next_sequence(directory: &Path, date: NaiveDate) -> u32 {
    let date = date.format("%Y-%m-%d").to_string();
    fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| sequence_from_name(&entry.file_name().to_string_lossy(), &date))
        .max()
        .map_or(0, |value| value.saturating_add(1))
}

fn sequence_from_name(name: &str, date: &str) -> Option<u32> {
    name.strip_prefix(&format!("{LOG_PREFIX}{date}-"))?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn compress_inactive(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_some_and(|value| value == "jsonl") {
            compress(&path)?;
        }
    }
    Ok(())
}

fn compress(path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let destination = PathBuf::from(format!("{}.gz", path.display()));
    let mut input = File::open(path)?;
    let output = File::create(&destination)?;
    let mut encoder = GzEncoder::new(output, Compression::fast());
    io::copy(&mut input, &mut encoder)?;
    encoder.finish()?.sync_all()?;
    fs::remove_file(path)
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
        let modified = entry.metadata()?.modified()?;
        if now.duration_since(modified).is_ok_and(|age| age > max_age) {
            fs::remove_file(entry.path()).with_context(|| {
                format!("cannot remove expired log: {}", entry.path().display())
            })?;
        }
    }
    Ok(())
}

fn enrich_and_redact(buffer: &[u8], run_id: &str) -> io::Result<Vec<u8>> {
    let mut record: serde_json::Value = serde_json::from_slice(buffer)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    redact(&mut record, None);
    let object = record
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "log record is not an object"))?;
    object.insert("schema_version".into(), 1.into());
    object.insert("run_id".into(), run_id.into());
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
    let mut encoded = serde_json::to_vec(&record).map_err(io::Error::other)?;
    encoded.push(b'\n');
    Ok(encoded)
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

    #[test]
    fn rotates_by_size_and_compresses_previous_file() {
        let directory = temp_directory();
        let writer = RollingWriter::new(WriterConfig {
            directory: directory.clone(),
            max_file_bytes: 1,
            retention_days: 14,
            compress_rotated: true,
            run_id: "test-run".into(),
        })
        .unwrap();
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
        };
        assert_eq!(health.write_errors(), 2);
    }

    #[test]
    fn adds_context_and_redacts_sensitive_fields() {
        let encoded = enrich_and_redact(
            br#"{"fields":{"event":"test","token":"secret","incident_id":"incident-1"}}"#,
            "run-1",
        )
        .unwrap();
        let record: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(record["schema_version"], 1);
        assert_eq!(record["run_id"], "run-1");
        assert_eq!(record["correlation_id"], "incident-1");
        assert_eq!(record["fields"]["token"], "[REDACTED]");
    }
}
