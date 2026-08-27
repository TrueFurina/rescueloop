use anyhow::{Context, Result};
use rescueloop_core::Incident;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::storage;

#[derive(Serialize, Deserialize)]
struct PendingObservation {
    schema_version: u16,
    incident: Incident,
}

pub struct Pending {
    pub path: PathBuf,
    pub incident: Incident,
}

pub async fn begin(incident_dir: &Path, incident: &Incident) -> Result<PathBuf> {
    let directory = journal_directory(incident_dir);
    tokio::fs::create_dir_all(&directory).await?;
    let path = directory.join(format!("{}.json", incident.id));
    let value = PendingObservation {
        schema_version: 1,
        incident: incident.clone(),
    };
    storage::create_durable(&path, &serde_json::to_vec(&value)?).await?;
    Ok(path)
}

pub async fn pending(incident_dir: &Path) -> Result<Vec<Pending>> {
    let directory = journal_directory(incident_dir);
    let Ok(mut entries) = tokio::fs::read_dir(&directory).await else {
        return Ok(Vec::new());
    };
    let mut paths = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if entry.path().extension().and_then(|value| value.to_str()) == Some("json") {
            paths.push(entry.path());
        }
    }
    paths.sort();
    let mut result = Vec::with_capacity(paths.len());
    for path in paths {
        let value: PendingObservation = serde_json::from_slice(&tokio::fs::read(&path).await?)
            .with_context(|| format!("invalid observation journal: {}", path.display()))?;
        if value.schema_version != 1 {
            anyhow::bail!(
                "unsupported observation journal schema at {}",
                path.display()
            )
        }
        result.push(Pending {
            path,
            incident: value.incident,
        });
    }
    Ok(result)
}

pub async fn complete(path: &Path) -> Result<()> {
    storage::remove_durable(path).await
}

fn journal_directory(incident_dir: &Path) -> PathBuf {
    incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join("observation-journal")
}
