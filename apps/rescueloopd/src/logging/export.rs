use anyhow::{Context, Result, bail};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;

const BATCH_SIZE: usize = 100;
const MAX_SPOOL_FILES: usize = 10_000;
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const WARN_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct ExportSink {
    spool_directory: PathBuf,
    notify: mpsc::Sender<()>,
    sequence: Arc<AtomicU64>,
}

pub struct ExportRuntime {
    pub sink: ExportSink,
    receiver: mpsc::Receiver<()>,
    config: ExportConfig,
}

struct ExportConfig {
    endpoint: String,
    headers: HeaderMap,
}

pub fn configure(log_directory: &Path) -> Result<Option<ExportRuntime>> {
    let Some(endpoint) = std::env::var("RESCUELOOP_OTLP_ENDPOINT")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    if !(endpoint.starts_with("https://") || endpoint.starts_with("http://")) {
        bail!("RESCUELOOP_OTLP_ENDPOINT must use http or https")
    }
    let headers = parse_headers(std::env::var("RESCUELOOP_OTLP_HEADERS").ok().as_deref())?;
    let spool_directory = log_directory.join("otlp-spool");
    fs::create_dir_all(&spool_directory)?;
    let (notify, receiver) = mpsc::channel(1);
    Ok(Some(ExportRuntime {
        sink: ExportSink {
            spool_directory,
            notify,
            sequence: Arc::new(AtomicU64::new(0)),
        },
        receiver,
        config: ExportConfig { endpoint, headers },
    }))
}

impl ExportSink {
    pub fn enqueue(&self, record: &[u8]) -> Result<()> {
        if is_export_internal(record) {
            return Ok(());
        }
        fs::create_dir_all(&self.spool_directory)?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let name = format!("{timestamp:020}-{}-{sequence:010}.json", std::process::id());
        let path = self.spool_directory.join(name);
        let temporary = path.with_extension("tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(record)?;
        file.sync_data()?;
        fs::rename(&temporary, &path)?;
        let _ = self.notify.try_send(());
        Ok(())
    }
}

pub fn spawn(mut runtime: ExportRuntime) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("valid OTLP HTTP client configuration");
        let mut backoff = Duration::from_secs(1);
        let mut last_warning: Option<Instant> = None;
        let mut suppressed_failures = 0_u64;
        tracing::info!(
            event = "export.started",
            protocol = "otlp-http-json",
            durable_spool = true,
            "OTLP log export started"
        );
        loop {
            tokio::select! {
                value = runtime.receiver.recv() => {
                    if value.is_none() {
                        return;
                    }
                }
                () = tokio::time::sleep(backoff) => {}
            }
            while runtime.receiver.try_recv().is_ok() {}
            let mut files = match pending_files(&runtime.sink.spool_directory) {
                Ok(files) => files,
                Err(error) => {
                    warn_rate_limited(
                        &mut last_warning,
                        &mut suppressed_failures,
                        &error.to_string(),
                    );
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            };
            if files.is_empty() {
                backoff = Duration::from_secs(1);
                continue;
            }
            if let Err(error) = trim_spool(&files) {
                warn_rate_limited(
                    &mut last_warning,
                    &mut suppressed_failures,
                    &error.to_string(),
                );
            }
            if files.len() > MAX_SPOOL_FILES {
                files.drain(..files.len() - MAX_SPOOL_FILES);
            }
            let batch_files = files.into_iter().take(BATCH_SIZE).collect::<Vec<_>>();
            let mut records = Vec::with_capacity(batch_files.len());
            for path in &batch_files {
                match fs::read(path) {
                    Ok(record) if serde_json::from_slice::<Value>(&record).is_ok() => {
                        records.push(record);
                    }
                    Ok(_) | Err(_) => {
                        let quarantine = path.with_extension("corrupt");
                        let _ = fs::rename(path, quarantine);
                    }
                }
            }
            if records.is_empty() {
                continue;
            }
            let result = client
                .post(&runtime.config.endpoint)
                .headers(runtime.config.headers.clone())
                .json(&otlp_payload(&records))
                .send()
                .await;
            match result {
                Ok(response) if response.status().is_success() => {
                    for path in &batch_files {
                        if path.exists() {
                            let _ = fs::remove_file(path);
                        }
                    }
                    tracing::debug!(
                        event = "export.completed",
                        records = records.len(),
                        "OTLP log batch exported"
                    );
                    backoff = Duration::from_secs(1);
                    last_warning = None;
                    suppressed_failures = 0;
                }
                Ok(response) => {
                    warn_rate_limited(
                        &mut last_warning,
                        &mut suppressed_failures,
                        &format!("OTLP endpoint returned {}", response.status()),
                    );
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                Err(error) => {
                    warn_rate_limited(
                        &mut last_warning,
                        &mut suppressed_failures,
                        &error.to_string(),
                    );
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    })
}

fn warn_rate_limited(
    last_warning: &mut Option<Instant>,
    suppressed_failures: &mut u64,
    error: &str,
) {
    if last_warning.is_none_or(|value| value.elapsed() >= WARN_INTERVAL) {
        tracing::warn!(
            event = "export.failed",
            error,
            suppressed_failures = *suppressed_failures,
            "OTLP log export failed; durable spool retained"
        );
        *last_warning = Some(Instant::now());
        *suppressed_failures = 0;
    } else {
        *suppressed_failures = (*suppressed_failures).saturating_add(1);
    }
}

fn pending_files(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut files = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "json")
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn trim_spool(files: &[PathBuf]) -> Result<()> {
    let excess = files.len().saturating_sub(MAX_SPOOL_FILES);
    for path in files.iter().take(excess) {
        fs::remove_file(path)
            .with_context(|| format!("cannot trim OTLP spool: {}", path.display()))?;
    }
    if excess > 0 {
        tracing::error!(
            event = "export.spool_trimmed",
            dropped_records = excess,
            "OTLP spool reached its safety limit"
        );
    }
    Ok(())
}

fn is_export_internal(record: &[u8]) -> bool {
    serde_json::from_slice::<Value>(record)
        .ok()
        .and_then(|record| {
            record
                .pointer("/fields/event")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|event| event.starts_with("export."))
}

fn parse_headers(value: Option<&str>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for pair in value.into_iter().flat_map(|value| value.split(',')) {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (name, value) = pair
            .split_once('=')
            .context("RESCUELOOP_OTLP_HEADERS must contain name=value pairs")?;
        headers.insert(
            HeaderName::from_str(name.trim()).context("invalid OTLP header name")?,
            HeaderValue::from_str(value.trim()).context("invalid OTLP header value")?,
        );
    }
    Ok(headers)
}

fn otlp_payload(records: &[Vec<u8>]) -> Value {
    let log_records = records
        .iter()
        .filter_map(|record| serde_json::from_slice::<Value>(record).ok())
        .map(|record| {
            let timestamp = record
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .and_then(|value| value.timestamp_nanos_opt())
                .unwrap_or_default()
                .max(0)
                .to_string();
            let message = record
                .pointer("/fields/message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let attributes = ["event", "correlation_id", "run_id", "target"]
                .into_iter()
                .filter_map(|key| {
                    let value = if key == "event" {
                        record.pointer("/fields/event")
                    } else {
                        record.get(key)
                    }?;
                    Some(json!({"key": key, "value": {"stringValue": value.as_str().unwrap_or_default()}}))
                })
                .collect::<Vec<_>>();
            json!({
                "timeUnixNano": timestamp,
                "severityText": record.get("level").and_then(Value::as_str).unwrap_or("INFO"),
                "body": {"stringValue": message},
                "attributes": attributes
            })
        })
        .collect::<Vec<_>>();
    json!({
        "resourceLogs": [{
            "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "rescueloop"}}]},
            "scopeLogs": [{
                "scope": {"name": "rescueloop.operational"},
                "logRecords": log_records
            }]
        }]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use uuid::Uuid;

    fn temp_directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!("rescueloop-export-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn runtime(endpoint: String, directory: &Path) -> ExportRuntime {
        let (notify, receiver) = mpsc::channel(1);
        ExportRuntime {
            sink: ExportSink {
                spool_directory: directory.to_path_buf(),
                notify,
                sequence: Arc::new(AtomicU64::new(0)),
            },
            receiver,
            config: ExportConfig {
                endpoint,
                headers: HeaderMap::new(),
            },
        }
    }

    #[test]
    fn builds_otlp_log_request() {
        let payload = otlp_payload(&[br#"{"timestamp":"2026-08-27T20:00:00Z","level":"WARN","run_id":"run-1","correlation_id":"incident-1","target":"rescueloop","fields":{"event":"source.retrying","message":"retry"}}"#.to_vec()]);
        assert_eq!(
            payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["body"]["stringValue"],
            "retry"
        );
    }

    #[test]
    fn parses_opt_in_headers() {
        let headers = parse_headers(Some("authorization=Bearer value,x-tenant=demo")).unwrap();
        assert_eq!(headers.len(), 2);
    }

    #[tokio::test]
    async fn posts_spooled_batch_and_removes_checkpoint() {
        let directory = temp_directory();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let runtime = runtime(format!("http://{address}/v1/logs"), &directory);
        runtime
            .sink
            .enqueue(br#"{"timestamp":"2026-08-27T20:00:00Z","level":"INFO","run_id":"run","correlation_id":"run","target":"test","fields":{"event":"test","message":"hello"}}"#)
            .unwrap();
        assert_eq!(pending_files(&directory).unwrap().len(), 1);
        let exporter = spawn(runtime);

        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..read]);
            if request.windows(12).any(|value| value == b"resourceLogs") {
                break;
            }
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !pending_files(&directory).unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        exporter.abort();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_records_before_notification() {
        let directory = temp_directory();
        let runtime = runtime("http://127.0.0.1:1/v1/logs".into(), &directory);
        runtime
            .sink
            .enqueue(br#"{"fields":{"event":"test"}}"#)
            .unwrap();
        assert_eq!(pending_files(&directory).unwrap().len(), 1);
        fs::remove_dir_all(directory).unwrap();
    }
}
