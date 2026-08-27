use async_trait::async_trait;
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchContext {
    pub executable: PathBuf,
    /// Arguments are absent unless the user explicitly allowed recording them.
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
        Self {
            schema_version: 1,
            id: Uuid::new_v4(),
            observed_at: Utc::now(),
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
        }
    }

    /// Excludes UUID, timestamps, PID, paths, raw addresses and local artifacts.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisRequest {
    pub schema_version: u16,
    pub incident: Incident,
    pub allowed_actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisResponse {
    pub summary: String,
    pub hypotheses: Vec<Hypothesis>,
    pub proposed_actions: Vec<ProposedAction>,
    pub needs_more_evidence: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hypothesis {
    pub cause: String,
    pub confidence: f32,
    pub evidence_indexes: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedAction {
    pub action_type: String,
    pub reason: String,
    pub parameters: Value,
    pub reversible: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum AnalysisError {
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    #[error("provider returned an invalid response: {0}")]
    Invalid(String),
}

#[async_trait]
pub trait AnalysisProvider: Send + Sync {
    fn name(&self) -> &str;
    async fn analyze(&self, request: &AnalysisRequest) -> Result<AnalysisResponse, AnalysisError>;
}

#[async_trait]
pub trait IncidentCollector: Send {
    fn name(&self) -> &str;
    async fn next_incident(&mut self) -> anyhow::Result<Incident>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incident() -> Incident {
        let mut value = Incident::detected(
            "windows",
            IncidentKind::Crash,
            "crash",
            Evidence {
                source: "wer".into(),
                summary: "raw".into(),
                artifact: Some(PathBuf::from("C:/Users/alice/report.wer")),
                fields: BTreeMap::new(),
            },
        );
        value.application_identity = Some(ApplicationIdentity {
            name: "Demo".into(),
            version: Some("1.0".into()),
            binary_sha256: Some("abc".into()),
            architecture: Some("x86_64".into()),
            ..Default::default()
        });
        value.normalized_failure.code = Some("c0000005".into());
        value
    }

    #[test]
    fn fingerprint_ignores_unstable_and_private_fields() {
        let first = incident();
        let mut second = first.clone();
        second.id = Uuid::new_v4();
        second.observed_at = Utc::now();
        second.message = "different raw message".into();
        second.evidence[0].artifact = Some(PathBuf::from("C:/Users/bob/other.wer"));
        assert_eq!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn fingerprint_changes_for_normalized_failure() {
        let first = incident();
        let mut second = first.clone();
        second.normalized_failure.faulting_module = Some("d3d9.dll".into());
        assert_ne!(first.fingerprint(), second.fingerprint());
    }
}
