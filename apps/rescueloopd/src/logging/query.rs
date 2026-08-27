use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use flate2::read::GzDecoder;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};

use super::log_directory;

pub struct LogQuery {
    pub lines: usize,
    pub follow: bool,
    pub level: Option<String>,
    pub event: Option<String>,
    pub correlation_id: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub verify: bool,
    pub output: LogOutput,
}

pub enum LogOutput {
    Pretty,
    Json,
}

struct Filters {
    level: Option<String>,
    event: Option<String>,
    correlation_id: Option<String>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
}

pub async fn run(incident_dir: &Path, query: LogQuery) -> Result<()> {
    let directory = log_directory(incident_dir);
    let filters = Filters::from_query(&query)?;
    let files = log_files(&directory)?;
    let mut all_records = Vec::new();
    for path in &files {
        all_records.extend(read_records(path)?);
    }
    all_records.sort_by_key(timestamp);
    if query.verify {
        let verified = verify_chains(&all_records)?;
        eprintln!("Verified {verified} hash-chained log record(s). ");
    }
    let mut recent = VecDeque::with_capacity(query.lines.min(10_000));
    for record in all_records {
        if filters.matches(&record) {
            if recent.len() == query.lines {
                recent.pop_front();
            }
            if query.lines > 0 {
                recent.push_back(record);
            }
        }
    }
    for record in recent {
        print_record(&record, &query.output)?;
    }
    if !query.follow {
        if let Some(path) = files.last() {
            eprintln!("Log file: {}", path.display());
        } else {
            eprintln!("No operational logs yet: {}", directory.display());
        }
        return Ok(());
    }
    follow(&directory, &filters, &query.output).await
}

impl Filters {
    fn from_query(query: &LogQuery) -> Result<Self> {
        Ok(Self {
            level: query.level.as_ref().map(|value| value.to_ascii_uppercase()),
            event: query.event.clone(),
            correlation_id: query.correlation_id.clone(),
            since: parse_time(query.since.as_deref(), "since")?,
            until: parse_time(query.until.as_deref(), "until")?,
        })
    }

    fn matches(&self, record: &Value) -> bool {
        self.level.as_ref().is_none_or(|expected| {
            record.get("level").and_then(Value::as_str) == Some(expected.as_str())
        }) && self.event.as_ref().is_none_or(|expected| {
            record.pointer("/fields/event").and_then(Value::as_str) == Some(expected.as_str())
        }) && self.correlation_id.as_ref().is_none_or(|expected| {
            record.get("correlation_id").and_then(Value::as_str) == Some(expected.as_str())
        }) && self
            .since
            .is_none_or(|since| timestamp(record).is_some_and(|value| value >= since))
            && self
                .until
                .is_none_or(|until| timestamp(record).is_some_and(|value| value <= until))
    }
}

fn parse_time(value: Option<&str>, name: &str) -> Result<Option<DateTime<Utc>>> {
    value
        .map(|value| {
            DateTime::parse_from_rfc3339(value)
                .map(|value| value.with_timezone(&Utc))
                .with_context(|| format!("--{name} must be an RFC 3339 timestamp"))
        })
        .transpose()
}

fn timestamp(record: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(record.get("timestamp")?.as_str()?)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn log_files(directory: &Path) -> Result<Vec<PathBuf>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut files = std::fs::read_dir(directory)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("rescueloop-")
                && matches!(
                    entry.path().extension().and_then(|value| value.to_str()),
                    Some("jsonl" | "gz")
                )
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn read_records(path: &Path) -> Result<Vec<Value>> {
    let file = File::open(path)?;
    let reader: Box<dyn Read> = if path.extension().is_some_and(|value| value == "gz") {
        Box::new(GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    BufReader::new(reader)
        .lines()
        .filter_map(|line| match line {
            Ok(line) if line.trim().is_empty() => None,
            Ok(line) => Some(serde_json::from_str(&line).context("invalid JSONL log record")),
            Err(error) => Some(Err(error.into())),
        })
        .collect()
}

async fn follow(directory: &Path, filters: &Filters, output: &LogOutput) -> Result<()> {
    let mut current: Option<PathBuf> = None;
    let mut offset = 0_u64;
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal?;
                return Ok(());
            }
            () = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
        let latest = log_files(directory)?
            .into_iter()
            .rfind(|path| path.extension().is_some_and(|value| value == "jsonl"));
        if latest != current {
            current = latest;
            offset = current
                .as_ref()
                .and_then(|path| path.metadata().ok())
                .map_or(0, |metadata| metadata.len());
            continue;
        }
        let Some(path) = &current else { continue };
        let mut file = File::open(path)?;
        if file.metadata()?.len() < offset {
            offset = 0;
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        while reader.read_line(&mut line)? > 0 {
            offset += line.len() as u64;
            if let Ok(record) = serde_json::from_str::<Value>(&line)
                && filters.matches(&record)
            {
                print_record(&record, output)?;
            }
            line.clear();
        }
    }
}

fn print_record(record: &Value, output: &LogOutput) -> Result<()> {
    match output {
        LogOutput::Json => println!("{}", serde_json::to_string(record)?),
        LogOutput::Pretty => {
            let timestamp = record
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or("-");
            let level = record.get("level").and_then(Value::as_str).unwrap_or("-");
            let event = record
                .pointer("/fields/event")
                .and_then(Value::as_str)
                .unwrap_or("-");
            let message = record
                .pointer("/fields/message")
                .and_then(Value::as_str)
                .unwrap_or("");
            let correlation = record
                .get("correlation_id")
                .and_then(Value::as_str)
                .unwrap_or("-");
            println!("{timestamp} {level:<5} {event:<30} [{correlation}] {message}");
        }
    }
    Ok(())
}

fn verify_chains(records: &[Value]) -> Result<usize> {
    let mut runs: HashMap<&str, Vec<&Value>> = HashMap::new();
    for record in records {
        let run_id = record
            .get("run_id")
            .and_then(Value::as_str)
            .context("log record has no run_id")?;
        runs.entry(run_id).or_default().push(record);
    }
    let mut verified = 0;
    for (run_id, mut records) in runs {
        records.sort_by_key(|record| record.get("sequence").and_then(Value::as_u64));
        let mut prior: Option<(u64, &str)> = None;
        for record in records {
            let sequence = record
                .get("sequence")
                .and_then(Value::as_u64)
                .context("log record has no sequence")?;
            let previous_hash = record
                .get("previous_hash")
                .and_then(Value::as_str)
                .context("log record has no previous_hash")?;
            let expected = record
                .get("record_hash")
                .and_then(Value::as_str)
                .context("log record has no record_hash")?;
            let mut canonical = record.clone();
            canonical
                .as_object_mut()
                .context("log record is not an object")?
                .remove("record_hash");
            let actual = format!("{:x}", Sha256::digest(serde_json::to_vec(&canonical)?));
            if actual != expected {
                anyhow::bail!("log hash mismatch for run {run_id} sequence {sequence}")
            }
            if let Some((prior_sequence, prior_hash)) = prior {
                if sequence != prior_sequence.saturating_add(1) || previous_hash != prior_hash {
                    anyhow::bail!("broken log chain for run {run_id} sequence {sequence}")
                }
            } else if sequence == 0 && !previous_hash.is_empty() {
                anyhow::bail!("invalid initial log hash for run {run_id}")
            }
            prior = Some((sequence, expected));
            verified += 1;
        }
    }
    Ok(verified)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Value {
        serde_json::json!({
            "timestamp": "2026-08-27T20:00:00Z",
            "level": "WARN",
            "correlation_id": "incident-1",
            "fields": {"event": "source.retrying"}
        })
    }

    #[test]
    fn filters_by_level_event_correlation_and_time() {
        let filters = Filters {
            level: Some("WARN".into()),
            event: Some("source.retrying".into()),
            correlation_id: Some("incident-1".into()),
            since: parse_time(Some("2026-08-27T19:00:00Z"), "since").unwrap(),
            until: parse_time(Some("2026-08-27T21:00:00Z"), "until").unwrap(),
        };
        assert!(filters.matches(&record()));
    }

    #[test]
    fn rejects_tampered_hash_chain() {
        let mut first = serde_json::json!({
            "run_id": "run", "sequence": 0, "previous_hash": "", "fields": {"event": "a"}
        });
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&first).unwrap()));
        first["record_hash"] = hash.into();
        assert_eq!(verify_chains(&[first.clone()]).unwrap(), 1);
        first["fields"]["event"] = "tampered".into();
        assert!(verify_chains(&[first]).is_err());
    }
}
