use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

use crate::Incident;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisRequest {
    pub schema_version: u16,
    pub incident: Incident,
    pub allowed_actions: Vec<String>,
    pub evidence_assessment: EvidenceAssessment,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EvidenceAssessment {
    pub completeness: f32,
    pub missing: Vec<String>,
    pub redacted_fields: u32,
    pub retained_evidence: usize,
}

impl AnalysisRequest {
    pub fn bounded(mut incident: Incident, allowed_actions: Vec<String>) -> Self {
        const ALLOWED_FIELDS: &[&str] = &[
            "container_id",
            "diagnostic_lines",
            "diagnostic_output",
            "duration_ms",
            "engine",
            "engine_error",
            "event",
            "event_id",
            "exit_code",
            "oom_killed",
            "process",
            "provider",
            "restart_loop",
            "service_id",
            "signal",
            "size_bytes",
        ];
        let mut redacted_fields = 0_u32;
        if incident.evidence.len() > 20 {
            redacted_fields += (incident.evidence.len() - 20) as u32;
            incident.evidence.drain(..incident.evidence.len() - 20);
        }
        for evidence in &mut incident.evidence {
            if evidence.artifact.take().is_some() {
                redacted_fields += 1;
            }
            let before = evidence.fields.len();
            evidence
                .fields
                .retain(|key, _| ALLOWED_FIELDS.contains(&key.as_str()));
            redacted_fields += (before - evidence.fields.len()) as u32;
            if let Some(Value::Array(lines)) = evidence.fields.get_mut("diagnostic_output") {
                lines.truncate(30);
                for line in lines {
                    if let Value::String(text) = line {
                        *text = text.chars().take(500).collect();
                    }
                }
            }
        }
        if let Some(context) = &mut incident.launch_context {
            if context.arguments.take().is_some() {
                redacted_fields += 1;
            }
            if context.working_directory.take().is_some() {
                redacted_fields += 1;
            }
            context.executable = context
                .executable
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_default();
        }
        let has_code = incident.normalized_failure.code.is_some();
        let has_diagnostics = incident.evidence.iter().any(|evidence| {
            evidence
                .fields
                .get("diagnostic_output")
                .is_some_and(|value| value.as_array().is_some_and(|values| !values.is_empty()))
                || evidence.fields.contains_key("diagnostic_lines")
        });
        let mut missing = Vec::new();
        if !has_code {
            missing.push("failure_code".into());
        }
        if !has_diagnostics {
            missing.push("diagnostic_output".into());
        }
        let completeness =
            (if has_code { 0.5 } else { 0.0 }) + (if has_diagnostics { 0.5 } else { 0.0 });
        let retained_evidence = incident.evidence.len();
        Self {
            schema_version: 2,
            incident,
            allowed_actions,
            evidence_assessment: EvidenceAssessment {
                completeness,
                missing,
                redacted_fields,
                retained_evidence,
            },
        }
    }
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

/// Source-oriented alias for incident collectors.
pub trait EventSource: IncidentCollector {}

impl<T: IncidentCollector + ?Sized> EventSource for T {}
