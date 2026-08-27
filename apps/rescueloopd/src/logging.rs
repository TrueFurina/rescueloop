use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing_subscriber::EnvFilter;

mod writer;

use writer::{LogHealth, RollingWriter, WriterConfig};

const DEFAULT_FILTER: &str = "info,hyper=warn,reqwest=warn,rustls=warn";
const DEFAULT_RETENTION_DAYS: usize = 14;
const DEFAULT_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

pub struct LogGuard {
    health: LogHealth,
}

pub fn init(incident_dir: &Path) -> Result<LogGuard> {
    let directory = log_directory(incident_dir);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("cannot create log directory: {}", directory.display()))?;
    let retention_days = retention_days();
    let config = WriterConfig {
        directory: directory.clone(),
        max_file_bytes: max_file_bytes(),
        retention_days,
        compress_rotated: true,
    };
    let appender = RollingWriter::new(config)?;
    let health = appender.health();
    let filter = std::env::var("RUST_LOG")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(EnvFilter::new)
        .unwrap_or_else(|| EnvFilter::new(DEFAULT_FILTER));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(appender)
        .json()
        .with_current_span(true)
        .with_span_list(true)
        .with_ansi(false)
        .try_init()
        .map_err(|error| anyhow::anyhow!("cannot initialize operational logging: {error}"))?;

    install_panic_hook();
    tracing::info!(
        event = "logging.initialized",
        directory = %directory.display(),
        retention_days,
        format = "jsonl",
        "Operational logging initialized"
    );
    Ok(LogGuard { health })
}

impl LogGuard {
    pub fn write_errors(&self) -> u64 {
        self.health.write_errors()
    }
}

pub fn log_directory(incident_dir: &Path) -> PathBuf {
    incident_dir.parent().unwrap_or(incident_dir).join("logs")
}

fn retention_days() -> usize {
    std::env::var("RESCUELOOP_LOG_RETENTION_DAYS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_RETENTION_DAYS)
}

fn max_file_bytes() -> u64 {
    std::env::var("RESCUELOOP_LOG_MAX_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value >= 1024)
        .unwrap_or(DEFAULT_MAX_FILE_BYTES)
}

pub async fn print_recent(incident_dir: &Path, line_limit: usize) -> Result<()> {
    let directory = log_directory(incident_dir);
    let mut entries = tokio::fs::read_dir(&directory)
        .await
        .with_context(|| format!("cannot read log directory: {}", directory.display()))?;
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_file()
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with("rescueloop-")
            && entry
                .path()
                .extension()
                .is_some_and(|value| value == "jsonl")
        {
            files.push(entry.path());
        }
    }
    files.sort();
    let Some(path) = files.last() else {
        println!("No operational logs yet: {}", directory.display());
        return Ok(());
    };
    let content = tokio::fs::read_to_string(path).await?;
    let lines: Vec<_> = content.lines().collect();
    for line in lines.iter().skip(lines.len().saturating_sub(line_limit)) {
        println!("{line}");
    }
    eprintln!("Log file: {}", path.display());
    Ok(())
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        tracing::error!(
            event = "runtime.panic",
            panic = %panic,
            "RescueLoop panicked"
        );
        previous(panic);
    }));
}

#[cfg(test)]
mod tests {
    use super::log_directory;
    use std::path::Path;

    #[test]
    fn stores_logs_next_to_incident_state() {
        assert_eq!(
            log_directory(Path::new("state/incidents")),
            Path::new("state/logs")
        );
    }
}
