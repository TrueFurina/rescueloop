use anyhow::{Context, Result};
use fs2::FileExt;
use rescueloop_core::Incident;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};
use tokio::fs;

use crate::storage;

pub(crate) async fn incidents(dir: &Path) -> Result<Vec<(Incident, PathBuf)>> {
    let paths = match incident_index(dir).await {
        Ok(index) => match index.paths_newest_first().await {
            Ok(paths) => paths,
            Err(error) => {
                tracing::warn!(%error, "incident index unavailable; reading JSON directly");
                incident_json_paths(dir).await?
            }
        },
        Err(error) => {
            tracing::warn!(%error, "incident index could not open; reading JSON directly");
            incident_json_paths(dir).await?
        }
    };
    load_incidents(dir, paths).await
}

/// Reads the JSON source of truth without opening, rebuilding, or quarantining the disposable index.
pub(crate) async fn incidents_read_only(dir: &Path) -> Result<Vec<(Incident, PathBuf)>> {
    let paths = incident_json_paths(dir).await?;
    load_incidents(dir, paths).await
}

async fn load_incidents(dir: &Path, paths: Vec<PathBuf>) -> Result<Vec<(Incident, PathBuf)>> {
    let mut result = Vec::new();
    for path in paths {
        if let Ok(bytes) = fs::read(&path).await
            && let Ok(incident) = serde_json::from_slice::<Incident>(&bytes)
        {
            result.push((incident, path));
        }
    }
    // Status changes live in the ledger, not incident JSON.
    // Reconcile them for all readers.
    if let Ok(entries) = rescueloop_ledger::load(&ledger_path(dir)).await {
        let latest: std::collections::HashMap<_, _> = entries
            .into_iter()
            .map(|entry| (entry.incident_id, entry.status))
            .collect();
        for (incident, _) in &mut result {
            if let Some(status) = latest.get(&incident.id) {
                incident.status = status.clone();
            }
        }
    }
    result.retain(|(incident, _)| {
        let from_system_watcher = incident.evidence.iter().any(|evidence| {
            matches!(
                evidence.source.as_str(),
                "macos-diagnostic-reports" | "windows-error-reporting"
            )
        });
        let is_self = incident
            .application
            .as_deref()
            .is_some_and(|name| name.to_ascii_lowercase().starts_with("rescueloop"));
        !(from_system_watcher && is_self)
    });
    result.sort_by_key(|item| std::cmp::Reverse(item.0.observed_at));
    Ok(result)
}

async fn incident_json_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let Ok(mut entries) = fs::read_dir(dir).await else {
        return Ok(paths);
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    Ok(paths)
}

pub(crate) async fn incident_index(dir: &Path) -> Result<rescueloop_index::IncidentIndex> {
    let state_root = dir.parent().unwrap_or(dir);
    rescueloop_index::IncidentIndex::open(state_root, dir).await
}

pub(crate) async fn print_incidents(dir: &Path) -> Result<()> {
    let values = incidents(dir).await?;
    if values.is_empty() {
        println!("No incidents detected yet.");
        return Ok(());
    }
    println!("{} incident(s):", values.len());
    for (index, (incident, _)) in values.iter().enumerate() {
        println!(
            "[{}] {} — {:?} — {:?} — {}",
            index + 1,
            incident
                .application
                .as_deref()
                .unwrap_or("unknown application"),
            incident.kind,
            incident.status,
            local_timestamp(incident.observed_at)
        );
    }
    Ok(())
}

pub(crate) fn local_timestamp(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    timestamp
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

pub(crate) async fn incident_and_path_by_number(
    dir: &Path,
    number: &str,
) -> Result<(Incident, PathBuf)> {
    let index: usize = number
        .parse()
        .context("incident number must be a positive integer")?;
    if index == 0 {
        anyhow::bail!("incident numbering starts at 1")
    }
    incidents(dir)
        .await?
        .into_iter()
        .nth(index - 1)
        .context("incident number is out of range")
}

pub(crate) async fn incident_by_number(dir: &Path, number: &str) -> Result<Incident> {
    Ok(incident_and_path_by_number(dir, number).await?.0)
}

pub(crate) async fn save_incident(dir: &Path, incident: &Incident) -> Result<(PathBuf, bool)> {
    fs::create_dir_all(dir).await?;
    let (_, occurrence_created) = save_occurrence(dir, incident).await?;
    let _store_lock = acquire_store_lock(dir).await?;
    let group_key = incident_group_key(incident);
    let candidates = grouping_candidates(dir, &group_key).await?;
    if !occurrence_created
        && let Some((_, path)) = candidates.iter().find(|(candidate, _)| {
            candidate.group_key == group_key || incident_group_key(candidate) == group_key
        })
    {
        tracing::debug!(
            event = "occurrence.duplicate",
            incident_id = %incident.id,
            "Duplicate occurrence ignored"
        );
        return Ok((path.clone(), false));
    }
    if let Some((mut existing, path)) = candidates.into_iter().find(|(candidate, _)| {
        (candidate.group_key == group_key || incident_group_key(candidate) == group_key)
            && !matches!(
                candidate.status,
                rescueloop_core::IncidentStatus::VerifiedFixed
                    | rescueloop_core::IncidentStatus::Superseded
            )
    }) {
        existing.group_key = group_key;
        existing.occurrence_count = existing.occurrence_count.max(1) + 1;
        existing.first_observed_at = existing.first_observed_at.or(Some(existing.observed_at));
        existing.last_observed_at = Some(incident.observed_at);
        existing.message = incident.message.clone();
        existing.kind = incident.kind.clone();
        existing.normalized_failure = incident.normalized_failure.clone();
        existing.evidence.extend(incident.evidence.clone());
        if existing.evidence.len() > 20 {
            existing.evidence.drain(..existing.evidence.len() - 20);
        }
        storage::replace_durable(&path, &serde_json::to_vec_pretty(&existing)?).await?;
        tracing::info!(
            event = "incident.updated",
            incident_id = %existing.id,
            occurrence_count = existing.occurrence_count,
            evidence_count = existing.evidence.len(),
            "Active incident updated"
        );
        if let Ok(index) = incident_index(dir).await
            && let Err(error) = index.upsert(&existing, &path).await
        {
            tracing::warn!(%error, "incident JSON saved but disposable index update failed");
        }
        return Ok((path, false));
    }
    let mut incident = incident.clone();
    incident.group_key = group_key;
    incident.occurrence_count = 1;
    incident.first_observed_at = Some(incident.observed_at);
    incident.last_observed_at = Some(incident.observed_at);
    let destination = dir.join(format!("{}.json", incident.id));
    if !storage::create_durable(&destination, &serde_json::to_vec_pretty(&incident)?).await? {
        return Ok((destination, false));
    }
    tracing::info!(
        event = "incident.created",
        incident_id = %incident.id,
        kind = ?incident.kind,
        evidence_count = incident.evidence.len(),
        "Incident JSON created"
    );
    if let Ok(index) = incident_index(dir).await
        && let Err(error) = index.upsert(&incident, &destination).await
    {
        tracing::warn!(%error, "incident JSON saved but disposable index update failed");
    }
    let entry = rescueloop_ledger::append(
        &ledger_path(dir),
        rescueloop_ledger::NewLedgerEntry {
            incident: incident.clone(),
            repair: None,
            before_state: None,
            after_state: None,
            verifier: None,
            status: incident.status.clone(),
            relation_override: None,
        },
    )
    .await?;
    tracing::info!(
        event = "lineage.appended",
        incident_id = %incident.id,
        relation = ?entry.relation,
        "Incident lineage appended"
    );
    println!("LINEAGE: {:?}", entry.relation);
    Ok((destination, true))
}

struct StoreLock(File);

async fn acquire_store_lock(incident_dir: &Path) -> Result<StoreLock> {
    let path = incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join(".incident-store.lock");
    tokio::task::spawn_blocking(move || {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        file.lock_exclusive()
            .with_context(|| format!("cannot lock incident store: {}", path.display()))?;
        Ok(StoreLock(file))
    })
    .await?
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

async fn grouping_candidates(dir: &Path, group_key: &str) -> Result<Vec<(Incident, PathBuf)>> {
    if let Ok(index) = incident_index(dir).await
        && let Ok(paths) = index.paths_for_group(group_key).await
        && !paths.is_empty()
    {
        return load_incidents(dir, paths).await;
    }
    // Older documents may predate persisted group keys. A one-time full scan
    // preserves compatibility; the first match is upgraded by save_incident.
    incidents(dir).await
}

async fn save_occurrence(incident_dir: &Path, incident: &Incident) -> Result<(PathBuf, bool)> {
    let state_root = incident_dir.parent().unwrap_or(incident_dir);
    let directory = state_root.join("occurrences");
    fs::create_dir_all(&directory).await?;
    let destination = directory.join(format!("{}.json", incident.id));
    let created =
        storage::create_durable(&destination, &serde_json::to_vec_pretty(incident)?).await?;
    if !created {
        return Ok((destination, false));
    }
    tracing::debug!(
        event = "occurrence.created",
        incident_id = %incident.id,
        "Immutable occurrence created"
    );
    Ok((destination, true))
}

fn incident_group_key(incident: &Incident) -> String {
    for evidence in &incident.evidence {
        let engine = evidence
            .fields
            .get("engine")
            .and_then(|value| value.as_str());
        let container = evidence
            .fields
            .get("container_id")
            .and_then(|value| value.as_str());
        if let (Some(engine), Some(container)) = (engine, container) {
            return format!("container:{engine}:{container}");
        }
    }
    incident.fingerprint()
}

pub(crate) fn ledger_path(incident_dir: &Path) -> PathBuf {
    incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join("repair-ledger.jsonl")
}

pub(crate) async fn dismiss_incident(incident_dir: &Path, incident: &Incident) -> Result<()> {
    record_incident_status(
        incident_dir,
        incident,
        rescueloop_core::IncidentStatus::Superseded,
        Some(serde_json::json!({"dismissed_by_user": true})),
    )
    .await
}

pub(crate) async fn record_incident_status(
    incident_dir: &Path,
    incident: &Incident,
    status: rescueloop_core::IncidentStatus,
    detail: Option<serde_json::Value>,
) -> Result<()> {
    let status_for_log = status.clone();
    rescueloop_ledger::append(
        &ledger_path(incident_dir),
        rescueloop_ledger::NewLedgerEntry {
            incident: incident.clone(),
            repair: None,
            before_state: None,
            after_state: detail,
            verifier: None,
            status,
            relation_override: None,
        },
    )
    .await?;
    tracing::info!(
        event = "incident.status_changed",
        incident_id = %incident.id,
        status = ?status_for_log,
        "Incident status recorded"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rescueloop_core::{Evidence, IncidentKind};
    use std::collections::BTreeMap;

    fn fixture(application: &str, code: &str) -> Incident {
        let mut incident = Incident::detected(
            "test",
            IncidentKind::Crash,
            "failure",
            Evidence {
                source: "test".into(),
                summary: "failure".into(),
                artifact: None,
                fields: BTreeMap::new(),
            },
        );
        incident.application = Some(application.into());
        incident.normalized_failure.code = Some(code.into());
        incident
    }

    #[tokio::test]
    async fn indexed_grouping_ignores_unrelated_broken_projection() {
        let root = std::env::temp_dir().join(format!("rescueloop-store-{}", uuid::Uuid::new_v4()));
        let directory = root.join("incidents");
        let first = fixture("api", "oom");
        let (first_path, created) = save_incident(&directory, &first).await.unwrap();
        assert!(created);
        let unrelated = fixture("worker", "panic");
        let (unrelated_path, created) = save_incident(&directory, &unrelated).await.unwrap();
        assert!(created);
        fs::write(&unrelated_path, b"broken unrelated JSON")
            .await
            .unwrap();

        let recurrence = fixture("api", "oom");
        let (grouped_path, created) = save_incident(&directory, &recurrence).await.unwrap();
        assert!(!created);
        assert_eq!(grouped_path, first_path);
        let grouped: Incident =
            serde_json::from_slice(&fs::read(first_path).await.unwrap()).unwrap();
        assert_eq!(grouped.occurrence_count, 2);
        fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_grouping_preserves_every_occurrence() {
        let root = std::env::temp_dir().join(format!("rescueloop-store-{}", uuid::Uuid::new_v4()));
        let directory = root.join("incidents");
        let tasks = (0..16)
            .map(|_| {
                let directory = directory.clone();
                tokio::spawn(async move { save_incident(&directory, &fixture("api", "oom")).await })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        let stored = incidents(&directory).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].0.occurrence_count, 16);
        let mut occurrences = fs::read_dir(root.join("occurrences")).await.unwrap();
        let mut occurrence_count = 0;
        while occurrences.next_entry().await.unwrap().is_some() {
            occurrence_count += 1;
        }
        assert_eq!(occurrence_count, 16);
        fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn duplicate_occurrence_is_idempotent() {
        let root = std::env::temp_dir().join(format!("rescueloop-store-{}", uuid::Uuid::new_v4()));
        let directory = root.join("incidents");
        let occurrence = fixture("api", "oom");
        save_incident(&directory, &occurrence).await.unwrap();
        let (_, created) = save_incident(&directory, &occurrence).await.unwrap();
        assert!(!created);

        let stored = incidents(&directory).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].0.occurrence_count, 1);
        fs::remove_dir_all(root).await.unwrap();
    }
}
