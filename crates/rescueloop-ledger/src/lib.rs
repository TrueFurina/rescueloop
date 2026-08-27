use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rescueloop_core::{Incident, IncidentStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CausalRelation {
    InitialFailure,
    LifecycleUpdate,
    Regression,
    IncompleteRepair,
    NewFailure,
    VerificationStale,
    AdverseEffect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewLedgerEntry {
    pub incident: Incident,
    pub repair: Option<Value>,
    pub before_state: Option<Value>,
    pub after_state: Option<Value>,
    pub verifier: Option<Value>,
    pub status: IncidentStatus,
    /// Only `AdverseEffect` requires an explicit causal assertion. Other values
    /// are derived from stable fingerprints and prior entries.
    pub relation_override: Option<CausalRelation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub schema_version: u16,
    pub id: Uuid,
    pub recorded_at: DateTime<Utc>,
    pub incident_id: Uuid,
    pub application_name: Option<String>,
    pub application_fingerprint: String,
    pub environment_fingerprint: String,
    pub incident_fingerprint: String,
    pub repair: Option<Value>,
    pub before_state: Option<Value>,
    pub after_state: Option<Value>,
    pub verifier: Option<Value>,
    pub status: IncidentStatus,
    pub relation: CausalRelation,
    pub related_entry: Option<Uuid>,
    pub previous_hash: Option<String>,
    pub entry_hash: String,
}

#[tracing::instrument(name = "ledger.load", skip_all, err)]
pub async fn load(path: &Path) -> Result<Vec<LedgerEntry>> {
    if !fs::try_exists(path).await? {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path).await?;
    let mut entries = Vec::new();
    let mut previous: Option<String> = None;
    for (index, line) in content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
    {
        let entry: LedgerEntry = serde_json::from_str(line)
            .with_context(|| format!("invalid ledger line {}", index + 1))?;
        if entry.previous_hash != previous {
            bail!("ledger hash-chain break at line {}", index + 1)
        }
        if calculate_hash(&entry)? != entry.entry_hash {
            bail!("ledger content tampering at line {}", index + 1)
        }
        previous = Some(entry.entry_hash.clone());
        entries.push(entry);
    }
    Ok(entries)
}

#[tracing::instrument(
    name = "ledger.append",
    skip(path, new),
    fields(incident_id = %new.incident.id, status = ?new.status),
    err
)]
pub async fn append(path: &Path, new: NewLedgerEntry) -> Result<LedgerEntry> {
    let prior = load(path).await?;
    let (relation, related_entry) = classify(&prior, &new);
    let mut entry = LedgerEntry {
        schema_version: 1,
        id: Uuid::new_v4(),
        recorded_at: Utc::now(),
        incident_id: new.incident.id,
        application_name: new
            .incident
            .application_identity
            .as_ref()
            .map(|x| x.name.clone())
            .or(new.incident.application.clone()),
        application_fingerprint: new.incident.application_fingerprint(),
        environment_fingerprint: new.incident.environment_fingerprint(),
        incident_fingerprint: new.incident.fingerprint(),
        repair: new.repair,
        before_state: new.before_state,
        after_state: new.after_state,
        verifier: new.verifier,
        status: new.status,
        relation,
        related_entry,
        previous_hash: prior.last().map(|x| x.entry_hash.clone()),
        entry_hash: String::new(),
    };
    entry.entry_hash = calculate_hash(&entry)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(&serde_json::to_vec(&entry)?).await?;
    file.write_all(b"\n").await?;
    file.flush().await?;
    Ok(entry)
}

fn classify(prior: &[LedgerEntry], new: &NewLedgerEntry) -> (CausalRelation, Option<Uuid>) {
    if new.relation_override == Some(CausalRelation::AdverseEffect) {
        return (CausalRelation::AdverseEffect, prior.last().map(|x| x.id));
    }
    let app_name = new
        .incident
        .application_identity
        .as_ref()
        .map(|x| x.name.as_str())
        .or(new.incident.application.as_deref());
    let Some(previous) = prior
        .iter()
        .rev()
        .find(|entry| entry.application_name.as_deref() == app_name)
    else {
        return (CausalRelation::InitialFailure, None);
    };
    if previous.incident_id == new.incident.id {
        return (CausalRelation::LifecycleUpdate, Some(previous.id));
    }
    let app_fp = new.incident.application_fingerprint();
    let env_fp = new.incident.environment_fingerprint();
    if previous.application_fingerprint != app_fp || previous.environment_fingerprint != env_fp {
        return (CausalRelation::VerificationStale, Some(previous.id));
    }
    if previous.incident_fingerprint != new.incident.fingerprint() {
        return (CausalRelation::NewFailure, Some(previous.id));
    }
    let relation = match previous.status {
        IncidentStatus::VerifiedFixed => CausalRelation::Regression,
        IncidentStatus::RepairApplied | IncidentStatus::VerificationPending => {
            CausalRelation::IncompleteRepair
        }
        _ => CausalRelation::Regression,
    };
    (relation, Some(previous.id))
}

fn calculate_hash(entry: &LedgerEntry) -> Result<String> {
    #[derive(Serialize)]
    struct Hashable<'a> {
        schema_version: u16,
        id: Uuid,
        recorded_at: DateTime<Utc>,
        incident_id: Uuid,
        application_name: &'a Option<String>,
        application_fingerprint: &'a str,
        environment_fingerprint: &'a str,
        incident_fingerprint: &'a str,
        repair: &'a Option<Value>,
        before_state: &'a Option<Value>,
        after_state: &'a Option<Value>,
        verifier: &'a Option<Value>,
        status: &'a IncidentStatus,
        relation: &'a CausalRelation,
        related_entry: &'a Option<Uuid>,
        previous_hash: &'a Option<String>,
    }
    let value = Hashable {
        schema_version: entry.schema_version,
        id: entry.id,
        recorded_at: entry.recorded_at,
        incident_id: entry.incident_id,
        application_name: &entry.application_name,
        application_fingerprint: &entry.application_fingerprint,
        environment_fingerprint: &entry.environment_fingerprint,
        incident_fingerprint: &entry.incident_fingerprint,
        repair: &entry.repair,
        before_state: &entry.before_state,
        after_state: &entry.after_state,
        verifier: &entry.verifier,
        status: &entry.status,
        relation: &entry.relation,
        related_entry: &entry.related_entry,
        previous_hash: &entry.previous_hash,
    };
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rescueloop_core::{ApplicationIdentity, Evidence, IncidentKind};
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn incident(code: &str, version: &str) -> Incident {
        let mut value = Incident::detected(
            "windows",
            IncidentKind::Crash,
            "failure",
            Evidence {
                source: "fixture".into(),
                summary: "fixture".into(),
                artifact: None,
                fields: BTreeMap::new(),
            },
        );
        value.application = Some("Demo".into());
        value.application_identity = Some(ApplicationIdentity {
            name: "Demo".into(),
            version: Some(version.into()),
            binary_sha256: Some(version.into()),
            ..Default::default()
        });
        value.normalized_failure.code = Some(code.into());
        value
    }

    fn new(incident: Incident, status: IncidentStatus) -> NewLedgerEntry {
        NewLedgerEntry {
            incident,
            repair: None,
            before_state: None,
            after_state: None,
            verifier: None,
            status,
            relation_override: None,
        }
    }

    #[tokio::test]
    async fn classifies_regression_new_failure_and_stale_verification() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("ledger.jsonl");
        let first = append(
            &path,
            new(incident("oom", "1"), IncidentStatus::VerifiedFixed),
        )
        .await
        .unwrap();
        assert_eq!(first.relation, CausalRelation::InitialFailure);
        let regression = append(&path, new(incident("oom", "1"), IncidentStatus::Detected))
            .await
            .unwrap();
        assert_eq!(regression.relation, CausalRelation::Regression);
        let other = append(
            &path,
            new(incident("access_violation", "1"), IncidentStatus::Detected),
        )
        .await
        .unwrap();
        assert_eq!(other.relation, CausalRelation::NewFailure);
        let updated = append(
            &path,
            new(incident("access_violation", "2"), IncidentStatus::Detected),
        )
        .await
        .unwrap();
        assert_eq!(updated.relation, CausalRelation::VerificationStale);
        assert_eq!(load(&path).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn detects_ledger_tampering() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("ledger.jsonl");
        append(
            &path,
            new(incident("oom", "1"), IncidentStatus::VerifiedFixed),
        )
        .await
        .unwrap();
        let content = fs::read_to_string(&path)
            .await
            .unwrap()
            .replace("verified_fixed", "rolled_back");
        fs::write(&path, content).await.unwrap();
        assert!(load(&path).await.is_err());
    }
}
