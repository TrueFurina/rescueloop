use anyhow::{Context, Result};
use rescueloop_core::Incident;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

use crate::storage;

const MAX_JOURNAL_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

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
        let file = tokio::fs::File::open(&path).await?;
        let mut reader = file.take(MAX_JOURNAL_DOCUMENT_BYTES + 1);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        if bytes.len() as u64 > MAX_JOURNAL_DOCUMENT_BYTES {
            anyhow::bail!("observation journal is oversized: {}", path.display())
        }
        let value: PendingObservation = serde_json::from_slice(&bytes)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_oversized_pending_transaction() {
        let root =
            std::env::temp_dir().join(format!("rescueloop-journal-{}", uuid::Uuid::new_v4()));
        let incidents = root.join("incidents");
        let directory = journal_directory(&incidents);
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let file = std::fs::File::create(directory.join("oversized.json")).unwrap();
        file.set_len(MAX_JOURNAL_DOCUMENT_BYTES + 1).unwrap();
        assert!(pending(&incidents).await.is_err());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
