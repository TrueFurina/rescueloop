use futures::StreamExt;
use rescueloop_core::AnalysisRequest;
use rmcp::{
    ServiceExt,
    handler::server::wrapper::{Json, Parameters},
    model::{CallToolResult, ServerInfo},
    tool, tool_router,
    transport::async_rw::JsonRpcMessageCodec,
};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio_util::codec::{FramedRead, FramedWrite};
use uuid::Uuid;

use crate::{incident_store::incidents_read_only, storage};

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
struct RescueLoopMcp {
    incident_dir: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListIncidentsInput {
    /// Maximum number of newest incidents to return. Range: 1 through 100.
    #[schemars(range(min = 1, max = 100), transform = remove_format)]
    #[serde(default = "default_limit")]
    limit: u32,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ListIncidentsOutput {
    incidents: Vec<IncidentSummary>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct IncidentSummary {
    id: String,
    observed_at: String,
    platform: String,
    kind: String,
    confidence: String,
    application: String,
    status: String,
    #[schemars(transform = remove_format)]
    occurrence_count: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetIncidentInput {
    /// UUID returned by list_incidents. Filesystem paths are not accepted.
    incident_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetIncidentOutput {
    #[schemars(transform = remove_format)]
    schema_version: u16,
    /// Bounded incident JSON after RescueLoop privacy redaction.
    incident: IncidentDetail,
    /// Completeness and redaction metadata for the returned evidence.
    evidence_assessment: EvidenceAssessment,
}

#[derive(Debug, Serialize, JsonSchema)]
struct IncidentDetail {
    id: String,
    observed_at: String,
    platform: String,
    kind: String,
    confidence: String,
    application: String,
    message: String,
    status: String,
    #[schemars(transform = remove_format)]
    occurrence_count: u64,
    first_observed_at: String,
    last_observed_at: String,
    launch_executable: String,
    application_identity: BTreeMap<String, String>,
    environment_identity: BTreeMap<String, String>,
    normalized_failure: BTreeMap<String, String>,
    evidence: Vec<RedactedEvidence>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RedactedEvidence {
    source: String,
    summary: String,
    /// Allowlisted values encoded as compact JSON strings to keep the MCP schema portable.
    fields: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct EvidenceAssessment {
    #[schemars(transform = remove_format)]
    completeness: f32,
    missing: Vec<String>,
    #[schemars(transform = remove_format)]
    redacted_fields: u32,
    #[schemars(transform = remove_format)]
    retained_evidence: usize,
}

fn default_limit() -> u32 {
    20
}

fn remove_format(schema: &mut Schema) {
    schema.remove("format");
}

pub async fn serve(incident_dir: &Path) -> anyhow::Result<()> {
    let incident_dir = storage::prepare_mcp_store(incident_dir)?;
    let reader = FramedRead::new(
        tokio::io::stdin(),
        JsonRpcMessageCodec::<rmcp::service::RxJsonRpcMessage<rmcp::RoleServer>>::new_with_max_length(
            MAX_MESSAGE_BYTES,
        ),
    )
    .filter_map(|result| futures::future::ready(match result {
        Ok(message) => Some(message),
        Err(error) => {
            tracing::warn!(event = "mcp.input_rejected", %error, "Rejected MCP input");
            None
        }
    }));
    let writer = FramedWrite::new(
        tokio::io::stdout(),
        JsonRpcMessageCodec::<rmcp::service::TxJsonRpcMessage<rmcp::RoleServer>>::default(),
    );
    let service = RescueLoopMcp { incident_dir }
        .serve((writer, reader))
        .await?;
    service.waiting().await?;
    Ok(())
}

#[tool_router]
impl RescueLoopMcp {
    #[tool(
        description = "List redacted summaries of locally stored RescueLoop incidents.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_incidents(
        &self,
        Parameters(input): Parameters<ListIncidentsInput>,
    ) -> Result<Json<ListIncidentsOutput>, CallToolResult> {
        let limit = input.limit;
        if !(1..=100).contains(&limit) {
            return Err(tool_error("limit must be between 1 and 100"));
        }
        let values = self.read_incidents().await?;
        let incidents = values
            .into_iter()
            .take(limit as usize)
            .map(|(incident, _)| IncidentSummary {
                id: incident.id.to_string(),
                observed_at: incident.observed_at.to_rfc3339(),
                platform: incident.platform,
                kind: camel_to_snake(&format!("{:?}", incident.kind)),
                confidence: format!("{:?}", incident.confidence).to_ascii_lowercase(),
                application: incident.application.unwrap_or_else(|| "unknown".into()),
                status: camel_to_snake(&format!("{:?}", incident.status)),
                occurrence_count: incident.occurrence_count,
            })
            .collect();
        Ok(Json(ListIncidentsOutput { incidents }))
    }

    #[tool(
        description = "Get one bounded and redacted incident by UUID. Local paths and launch arguments are removed.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_incident(
        &self,
        Parameters(input): Parameters<GetIncidentInput>,
    ) -> Result<Json<GetIncidentOutput>, CallToolResult> {
        let id = Uuid::parse_str(&input.incident_id)
            .map_err(|_| tool_error("incident_id must be a UUID"))?;
        let incident = self
            .read_incidents()
            .await?
            .into_iter()
            .find_map(|(incident, _)| (incident.id == id).then_some(incident))
            .ok_or_else(|| tool_error("incident not found"))?;
        let packet = AnalysisRequest::bounded(incident, Vec::new());
        Ok(Json(GetIncidentOutput {
            schema_version: packet.schema_version,
            incident: IncidentDetail::from(packet.incident),
            evidence_assessment: EvidenceAssessment {
                completeness: packet.evidence_assessment.completeness,
                missing: packet.evidence_assessment.missing,
                redacted_fields: packet.evidence_assessment.redacted_fields,
                retained_evidence: packet.evidence_assessment.retained_evidence,
            },
        }))
    }

    async fn read_incidents(
        &self,
    ) -> Result<Vec<(rescueloop_core::Incident, PathBuf)>, CallToolResult> {
        incidents_read_only(&self.incident_dir).await.map_err(|error| {
            tracing::error!(event = "mcp.store_read_failed", %error, "MCP incident read failed");
            tool_error("the local incident store is unavailable or failed integrity checks")
        })
    }
}

impl From<rescueloop_core::Incident> for IncidentDetail {
    fn from(incident: rescueloop_core::Incident) -> Self {
        let launch_executable = incident
            .launch_context
            .as_ref()
            .map(|context| context.executable.to_string_lossy().into_owned())
            .unwrap_or_default();
        let application_identity = incident
            .application_identity
            .as_ref()
            .map(|identity| string_map(serde_json::to_value(identity).unwrap_or_default()))
            .unwrap_or_default();
        let environment_identity = incident
            .environment_identity
            .as_ref()
            .map(|identity| string_map(serde_json::to_value(identity).unwrap_or_default()))
            .unwrap_or_default();
        let normalized_failure =
            string_map(serde_json::to_value(&incident.normalized_failure).unwrap_or_default());
        Self {
            id: incident.id.to_string(),
            observed_at: incident.observed_at.to_rfc3339(),
            platform: incident.platform,
            kind: camel_to_snake(&format!("{:?}", incident.kind)),
            confidence: format!("{:?}", incident.confidence).to_ascii_lowercase(),
            application: incident.application.unwrap_or_else(|| "unknown".into()),
            message: incident.message,
            status: camel_to_snake(&format!("{:?}", incident.status)),
            occurrence_count: incident.occurrence_count,
            first_observed_at: incident
                .first_observed_at
                .map(|value| value.to_rfc3339())
                .unwrap_or_default(),
            last_observed_at: incident
                .last_observed_at
                .map(|value| value.to_rfc3339())
                .unwrap_or_default(),
            launch_executable,
            application_identity,
            environment_identity,
            normalized_failure,
            evidence: incident
                .evidence
                .into_iter()
                .map(|evidence| RedactedEvidence {
                    source: evidence.source,
                    summary: evidence.summary,
                    fields: evidence
                        .fields
                        .into_iter()
                        .map(|(key, value)| (key, compact_json(value)))
                        .collect(),
                })
                .collect(),
        }
    }
}

fn string_map(value: Value) -> BTreeMap<String, String> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.clone(), compact_json(value.clone())))
        .collect()
}

fn compact_json(value: Value) -> String {
    match value {
        Value::String(value) => value,
        other => {
            serde_json::to_string(&other).expect("serializing a serde_json::Value cannot fail")
        }
    }
}

#[rmcp::tool_handler]
impl rmcp::ServerHandler for RescueLoopMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default()
            .with_server_info(rmcp::model::Implementation::new(
                "rescueloop",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Read-only access to locally stored, redacted RescueLoop incidents. No repair, replay, arbitrary file, or shell tools are exposed.",
            );
        info.capabilities = rmcp::model::ServerCapabilities::builder()
            .enable_tools()
            .build();
        info
    }
}

fn tool_error(message: &str) -> CallToolResult {
    CallToolResult::error(vec![rmcp::model::ContentBlock::text(message.to_owned())])
}

fn camel_to_snake(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for (index, character) in value.chars().enumerate() {
        if character.is_ascii_uppercase() && index > 0 {
            result.push('_');
        }
        result.push(character.to_ascii_lowercase());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use rescueloop_core::{Evidence, Incident, IncidentKind, LaunchContext};
    use rmcp::model::CallToolRequestParams;
    use std::collections::BTreeMap;
    use tokio::io::duplex;
    use tokio_util::codec::Decoder;

    async fn connected_server(
        incident_dir: PathBuf,
    ) -> anyhow::Result<rmcp::service::RunningService<rmcp::RoleClient, ()>> {
        let (client_transport, server_transport) = duplex(64 * 1024);
        tokio::spawn(async move {
            RescueLoopMcp { incident_dir }
                .serve(server_transport)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        Ok(().serve(client_transport).await?)
    }

    #[test]
    fn exposes_only_read_only_tools() {
        let tools = RescueLoopMcp::tool_router().list_all();
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().all(|tool| tool.output_schema.is_some()));
        assert!(tools.iter().all(|tool| {
            tool.annotations
                .as_ref()
                .and_then(|annotations| annotations.read_only_hint)
                == Some(true)
        }));
        assert!(!tools.iter().any(|tool| tool.name.contains("repair")));
        let info = <RescueLoopMcp as rmcp::ServerHandler>::get_info(&RescueLoopMcp {
            incident_dir: PathBuf::from("unused"),
        });
        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.resources.is_none());
    }

    #[test]
    fn rejects_oversized_protocol_messages() {
        let mut codec = JsonRpcMessageCodec::<
            rmcp::service::RxJsonRpcMessage<rmcp::RoleServer>,
        >::new_with_max_length(MAX_MESSAGE_BYTES);
        let mut input = bytes::BytesMut::from(vec![b'x'; MAX_MESSAGE_BYTES + 1].as_slice());
        input.extend_from_slice(b"\n");
        assert!(codec.decode(&mut input).is_err());
    }

    #[tokio::test]
    async fn validates_arguments_and_returns_tool_errors() {
        let root = test_root();
        tokio::fs::create_dir_all(&root).await.unwrap();
        let client = connected_server(root.clone()).await.unwrap();
        let invalid = client
            .call_tool(CallToolRequestParams::new("list_incidents").with_arguments(
                serde_json::Map::from_iter([("limit".into(), Value::String("bad".into()))]),
            ))
            .await
            .unwrap();
        assert_eq!(invalid.is_error, Some(true));
        let unknown = client
            .call_tool(CallToolRequestParams::new("list_incidents").with_arguments(
                serde_json::Map::from_iter([("unexpected".into(), Value::Bool(true))]),
            ))
            .await
            .unwrap();
        assert_eq!(unknown.is_error, Some(true));
        let missing = client
            .call_tool(CallToolRequestParams::new("get_incident").with_arguments(
                serde_json::Map::from_iter([(
                    "incident_id".into(),
                    Value::String(Uuid::new_v4().to_string()),
                )]),
            ))
            .await
            .unwrap();
        assert_eq!(missing.is_error, Some(true));
        client.cancel().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn returns_object_structured_content_and_redacts_private_data() {
        let state_root = test_root();
        let incident_dir = state_root.join("incidents");
        tokio::fs::create_dir_all(&incident_dir).await.unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("private_home".into(), serde_json::json!("/Users/alice"));
        fields.insert("exit_code".into(), serde_json::json!(1));
        let mut incident = Incident::detected(
            "test",
            IncidentKind::Crash,
            "failure",
            Evidence {
                source: "test".into(),
                summary: "test evidence".into(),
                artifact: Some(PathBuf::from("/Users/alice/private.crash")),
                fields,
            },
        );
        incident.launch_context = Some(LaunchContext {
            executable: PathBuf::from("/Users/alice/bin/private-app"),
            arguments: Some(vec!["--token=secret".into()]),
            working_directory: Some(PathBuf::from("/Users/alice/private")),
        });
        tokio::fs::write(
            incident_dir.join(format!("{}.json", incident.id)),
            serde_json::to_vec(&incident).unwrap(),
        )
        .await
        .unwrap();
        let client = connected_server(incident_dir).await.unwrap();
        let listed = client
            .call_tool(CallToolRequestParams::new("list_incidents"))
            .await
            .unwrap();
        assert!(listed.structured_content.unwrap().is_object());
        let detail = client
            .call_tool(CallToolRequestParams::new("get_incident").with_arguments(
                serde_json::Map::from_iter([(
                    "incident_id".into(),
                    Value::String(incident.id.to_string()),
                )]),
            ))
            .await
            .unwrap()
            .structured_content
            .unwrap();
        assert!(detail.is_object());
        assert!(detail["incident"]["evidence"][0].get("artifact").is_none());
        assert!(
            detail["incident"]["evidence"][0]["fields"]
                .get("private_home")
                .is_none()
        );
        assert_eq!(detail["incident"]["launch_executable"], "private-app");
        let serialized = serde_json::to_string(&detail).unwrap();
        assert!(!serialized.contains("--token=secret"));
        assert!(!serialized.contains("/Users/alice/private"));
        assert!(
            !state_root.join("index-v1.db").exists(),
            "read-only MCP calls must not create the disposable index"
        );
        client.cancel().await.unwrap();
        std::fs::remove_dir_all(state_root).unwrap();
    }

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!("rescueloop-mcp-test-{}", Uuid::new_v4()))
    }
}
