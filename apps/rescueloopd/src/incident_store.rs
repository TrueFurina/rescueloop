use anyhow::{Context, Result};
use fs2::FileExt;
use rescueloop_core::Incident;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};
use tokio::fs;
use tokio::io::AsyncReadExt;

use crate::{observation_journal, storage};

const MAX_INCIDENT_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

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
        if let Ok(bytes) = read_bounded_document(&path, MAX_INCIDENT_DOCUMENT_BYTES).await
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

async fn read_bounded_document(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = fs::File::open(path).await?;
    let mut reader = file.take(limit + 1);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > limit {
        anyhow::bail!(
            "incident document exceeds {} bytes: {}",
            limit,
            path.display()
        )
    }
    Ok(bytes)
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
    let _store_lock = acquire_store_lock(dir).await?;
    recover_pending_locked(dir).await?;
    if occurrence_path(dir, incident.id).exists() {
        let group_key = incident_group_key(incident);
        let grouping = grouping_candidates(dir, &group_key).await?;
        if let Some((_, path)) = grouping.incidents.iter().find(|(candidate, _)| {
            candidate.group_key == group_key || incident_group_key(candidate) == group_key
        }) {
            tracing::debug!(
                event = "occurrence.duplicate",
                incident_id = %incident.id,
                "Duplicate occurrence ignored"
            );
            return Ok((path.clone(), false));
        }
    }
    let journal = observation_journal::begin(dir, incident).await?;
    let result = apply_observation(dir, incident).await?;
    observation_journal::complete(&journal).await?;
    Ok(result)
}

pub(crate) async fn recover_pending_observations(dir: &Path) -> Result<usize> {
    fs::create_dir_all(dir).await?;
    let _store_lock = acquire_store_lock(dir).await?;
    recover_pending_locked(dir).await
}

async fn recover_pending_locked(dir: &Path) -> Result<usize> {
    let pending = observation_journal::pending(dir).await?;
    let count = pending.len();
    for transaction in pending {
        apply_observation(dir, &transaction.incident).await?;
        observation_journal::complete(&transaction.path).await?;
        tracing::warn!(
            event = "observation.recovered",
            incident_id = %transaction.incident.id,
            "Recovered interrupted observation transaction"
        );
    }
    Ok(count)
}

async fn apply_observation(dir: &Path, incident: &Incident) -> Result<(PathBuf, bool)> {
    save_occurrence(dir, incident).await?;
    abort_after_occurrence_if_requested();
    let group_key = incident_group_key(incident);
    let grouping = grouping_candidates(dir, &group_key).await?;
    let candidates = grouping.incidents;
    if let Some((existing, path)) = candidates
        .iter()
        .find(|(candidate, _)| candidate.last_occurrence_id == Some(incident.id))
    {
        ensure_initial_lineage(dir, existing).await?;
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
        existing.last_occurrence_id = Some(incident.id);
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
        if let Some(index) = grouping.index
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
    incident.last_occurrence_id = Some(incident.id);
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
    if let Some(index) = grouping.index
        && let Err(error) = index.upsert(&incident, &destination).await
    {
        tracing::warn!(%error, "incident JSON saved but disposable index update failed");
    }
    ensure_initial_lineage(dir, &incident).await?;
    Ok((destination, true))
}

#[cfg(debug_assertions)]
fn abort_after_occurrence_if_requested() {
    if std::env::var("RESCUELOOP_TEST_ABORT_AFTER_OCCURRENCE").as_deref() == Ok("1") {
        std::process::abort();
    }
}

#[cfg(not(debug_assertions))]
fn abort_after_occurrence_if_requested() {}

async fn ensure_initial_lineage(dir: &Path, incident: &Incident) -> Result<()> {
    let ledger = ledger_path(dir);
    let entry = rescueloop_ledger::append_if_missing(
        &ledger,
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
    let Some(entry) = entry else {
        return Ok(());
    };
    tracing::info!(
        event = "lineage.appended",
        incident_id = %incident.id,
        relation = ?entry.relation,
        "Incident lineage appended"
    );
    println!("LINEAGE: {:?}", entry.relation);
    Ok(())
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

struct GroupingCandidates {
    incidents: Vec<(Incident, PathBuf)>,
    index: Option<rescueloop_index::IncidentIndex>,
}

async fn grouping_candidates(dir: &Path, group_key: &str) -> Result<GroupingCandidates> {
    let index = incident_index(dir).await.ok();
    if let Some(index) = &index
        && let Ok(paths) = index.paths_for_group(group_key).await
        && !paths.is_empty()
    {
        return Ok(GroupingCandidates {
            incidents: load_incidents(dir, paths).await?,
            index: Some(index.clone()),
        });
    }
    // Older documents may predate persisted group keys. A one-time full scan
    // preserves compatibility; the first match is upgraded by save_incident.
    Ok(GroupingCandidates {
        incidents: load_incidents(dir, incident_json_paths(dir).await?).await?,
        index,
    })
}

fn occurrence_path(incident_dir: &Path, incident_id: uuid::Uuid) -> PathBuf {
    incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join("occurrences")
        .join(format!("{incident_id}.json"))
}

async fn save_occurrence(incident_dir: &Path, incident: &Incident) -> Result<PathBuf> {
    let destination = occurrence_path(incident_dir, incident.id);
    let directory = destination
        .parent()
        .context("occurrence path has no parent")?;
    fs::create_dir_all(&directory).await?;
    let created =
        storage::create_durable(&destination, &serde_json::to_vec_pretty(incident)?).await?;
    if !created {
        return Ok(destination);
    }
    tracing::debug!(
        event = "occurrence.created",
        incident_id = %incident.id,
        "Immutable occurrence created"
    );
    Ok(destination)
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
mod tests;
