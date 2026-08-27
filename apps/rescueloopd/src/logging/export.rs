use anyhow::{Context, Result, bail};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::{str::FromStr, time::Duration};
use tokio::sync::mpsc;

const CHANNEL_CAPACITY: usize = 1_024;
const BATCH_SIZE: usize = 100;

pub struct ExportRuntime {
    pub sender: mpsc::Sender<Vec<u8>>,
    receiver: mpsc::Receiver<Vec<u8>>,
    config: ExportConfig,
}

struct ExportConfig {
    endpoint: String,
    headers: HeaderMap,
}

pub fn configure() -> Result<Option<ExportRuntime>> {
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
    let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
    Ok(Some(ExportRuntime {
        sender,
        receiver,
        config: ExportConfig { endpoint, headers },
    }))
}

pub fn spawn(mut runtime: ExportRuntime) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        tracing::info!(
            event = "export.started",
            protocol = "otlp-http-json",
            "OTLP log export started"
        );
        while let Some(first) = runtime.receiver.recv().await {
            let mut batch = vec![first];
            let deadline = tokio::time::sleep(Duration::from_secs(1));
            tokio::pin!(deadline);
            while batch.len() < BATCH_SIZE {
                tokio::select! {
                    () = &mut deadline => break,
                    value = runtime.receiver.recv() => match value {
                        Some(value) => batch.push(value),
                        None => break,
                    }
                }
            }
            let payload = otlp_payload(&batch);
            match client
                .post(&runtime.config.endpoint)
                .headers(runtime.config.headers.clone())
                .json(&payload)
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    tracing::debug!(
                        event = "export.completed",
                        records = batch.len(),
                        "OTLP log batch exported"
                    );
                }
                Ok(response) => tracing::warn!(
                    event = "export.failed",
                    status = %response.status(),
                    records = batch.len(),
                    "OTLP endpoint rejected log batch"
                ),
                Err(error) => tracing::warn!(
                    event = "export.failed",
                    error = %error,
                    records = batch.len(),
                    "OTLP log export failed"
                ),
            }
        }
    })
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
    async fn posts_otlp_batch_to_configured_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel(4);
        let exporter = spawn(ExportRuntime {
            sender: sender.clone(),
            receiver,
            config: ExportConfig {
                endpoint: format!("http://{address}/v1/logs"),
                headers: HeaderMap::new(),
            },
        });
        sender
            .send(br#"{"timestamp":"2026-08-27T20:00:00Z","level":"INFO","run_id":"run","correlation_id":"run","target":"test","fields":{"event":"test","message":"hello"}}"#.to_vec())
            .await
            .unwrap();

        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|value| value == b"\r\n\r\n")
                && request.windows(12).any(|value| value == b"resourceLogs")
            {
                break;
            }
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        assert!(request.windows(12).any(|value| value == b"resourceLogs"));
        exporter.abort();
    }
}
