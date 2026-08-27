use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IncidentKind {
    Crash,
    Hang,
    AbnormalExit,
    ContainerExit,
    RestartLoop,
    OutOfMemory,
    Unhealthy,
    InstallerFailure,
    ServiceFailure,
    ResourceTermination,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Confirmed,
    Probable,
    Uncertain,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum IncidentStatus {
    #[default]
    Detected,
    Investigating,
    Diagnosed,
    RepairProposed,
    RepairApplied,
    VerificationPending,
    VerifiedFixed,
    VerificationFailed,
    RolledBack,
    Regressed,
    Superseded,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApplicationIdentity {
    pub name: String,
    pub version: Option<String>,
    pub binary_sha256: Option<String>,
    pub signature: Option<String>,
    pub architecture: Option<String>,
    pub runtime: Option<String>,
    #[serde(default)]
    pub plugins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EnvironmentIdentity {
    pub os: String,
    pub os_version: Option<String>,
    pub architecture: Option<String>,
    pub compatibility_layer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NormalizedFailure {
    pub code: Option<String>,
    pub faulting_module: Option<String>,
    pub stack_bucket: Option<String>,
    pub resource_bucket: Option<String>,
    #[serde(default)]
    pub missing_evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub source: String,
    pub summary: String,
    pub artifact: Option<PathBuf>,
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incident {
    pub schema_version: u16,
    pub id: Uuid,
    pub observed_at: DateTime<Utc>,
    pub platform: String,
    pub kind: IncidentKind,
    pub confidence: Confidence,
    pub application: Option<String>,
    pub message: String,
    pub evidence: Vec<Evidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_context: Option<LaunchContext>,
    #[serde(default)]
    pub status: IncidentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_identity: Option<ApplicationIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_identity: Option<EnvironmentIdentity>,
    #[serde(default)]
    pub normalized_failure: NormalizedFailure,
    #[serde(default)]
    pub group_key: String,
    #[serde(default = "default_occurrence_count")]
    pub occurrence_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_observed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observed_at: Option<DateTime<Utc>>,
}

fn default_occurrence_count() -> u64 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchContext {
    pub executable: PathBuf,
    /// Present only when recording was allowed.
    pub arguments: Option<Vec<String>>,
    pub working_directory: Option<PathBuf>,
}

impl Incident {
    pub fn detected(
        platform: impl Into<String>,
        kind: IncidentKind,
        message: impl Into<String>,
        evidence: Evidence,
    ) -> Self {
        let observed_at = Utc::now();
        Self {
            schema_version: 1,
            id: Uuid::new_v4(),
            observed_at,
            platform: platform.into(),
            kind,
            confidence: Confidence::Confirmed,
            application: None,
            message: message.into(),
            evidence: vec![evidence],
            launch_context: None,
            status: IncidentStatus::Detected,
            application_identity: None,
            environment_identity: Some(EnvironmentIdentity {
                os: std::env::consts::OS.into(),
                architecture: Some(std::env::consts::ARCH.into()),
                ..Default::default()
            }),
            normalized_failure: NormalizedFailure::default(),
            group_key: String::new(),
            occurrence_count: 1,
            first_observed_at: Some(observed_at),
            last_observed_at: Some(observed_at),
        }
    }

    /// Excludes unstable and private fields.
    pub fn fingerprint(&self) -> String {
        hash_json(&(
            &self.application_identity,
            &self.application,
            &self.environment_identity,
            &self.platform,
            &self.kind,
            &self.normalized_failure,
        ))
    }

    pub fn application_fingerprint(&self) -> String {
        hash_json(&(&self.application_identity, &self.application))
    }

    pub fn environment_fingerprint(&self) -> String {
        hash_json(&(&self.environment_identity, &self.platform))
    }
}

fn hash_json<T: Serialize>(value: &T) -> String {
    let encoded = serde_json::to_vec(value).expect("IR serialization cannot fail");
    format!("{:x}", Sha256::digest(encoded))
}
