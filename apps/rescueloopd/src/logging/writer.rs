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
            state: self.state.lock().unwrap_or_else(|error| error.into_inner()),
            health: &self.health,
        }
    }
}

pub struct EventWriter<'a> {
    state: MutexGuard<'a, State>,
    health: &'a LogHealth,
}

impl Write for EventWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self.state.write(buffer) {
            Ok(size) => Ok(size),
            Err(error) => {
                self.health.write_errors.fetch_add(1, Ordering::Relaxed);
                let _ = writeln!(io::stderr(), "RescueLoop log write failed: {error}");
                Err(error)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.state.flush()
    }
}

impl State {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let today = Local::now().date_naive();
        if today != self.date
            || self.bytes_written.saturating_add(buffer.len() as u64) > self.config.max_file_bytes
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
            max_file_bytes: 8,
            retention_days: 14,
            compress_rotated: true,
        })
        .unwrap();
        writer.make_writer().write_all(b"12345678").unwrap();
        writer.make_writer().write_all(b"next").unwrap();

        let compressed = fs::read_dir(&directory)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|value| value.path()))
            .find(|path| path.extension().is_some_and(|value| value == "gz"))
            .unwrap();
        let mut decoded = String::new();
        flate2::read::GzDecoder::new(File::open(&compressed).unwrap())
            .read_to_string(&mut decoded)
            .unwrap();
        assert_eq!(decoded, "12345678");
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn records_write_health() {
        let health = LogHealth {
            write_errors: Arc::new(AtomicU64::new(2)),
        };
        assert_eq!(health.write_errors(), 2);
    }
}
