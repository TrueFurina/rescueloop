use async_trait::async_trait;
use rescueloop_core::{AnalysisError, AnalysisProvider, AnalysisRequest, AnalysisResponse};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, process::Stdio};
use tokio::{io::AsyncWriteExt, process::Command};

mod validation;

pub use validation::{ALLOWED_ACTIONS, validate};

/// Provider-neutral JSON-over-HTTP adapter.
pub struct HttpAnalysisProvider {
    client: reqwest::Client,
    endpoint: String,
    bearer_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CliAgentKind {
    Codex,
    Claude,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub schema_version: u16,
    pub agent: CliAgentKind,
    pub executable: PathBuf,
}

pub fn detect_cli_agents() -> Vec<AgentConfig> {
    let mut detected = Vec::new();
    if let Some(executable) = find_executable("codex").or_else(find_bundled_codex) {
        detected.push(AgentConfig {
            schema_version: 1,
            agent: CliAgentKind::Codex,
            executable,
        });
    }
    if let Some(executable) = find_executable("claude") {
        detected.push(AgentConfig {
            schema_version: 1,
            agent: CliAgentKind::Claude,
            executable,
        });
    }
    detected
}

fn find_executable(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn find_bundled_codex() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "macos-aarch64",
        ("macos", "x86_64") => "macos-x86_64",
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        _ => return None,
    };
    let binary = if cfg!(windows) { "codex.exe" } else { "codex" };
    let extension_roots = [
        home.join(".vscode/extensions"),
        home.join(".vscode-insiders/extensions"),
        home.join(".cursor/extensions"),
    ];
    let mut candidates = Vec::new();
    for root in extension_roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with("openai.chatgpt-")
            {
                continue;
            }
            let candidate = entry.path().join("bin").join(platform).join(binary);
            if candidate.is_file() {
                let modified = candidate.metadata().and_then(|value| value.modified()).ok();
                candidates.push((modified, candidate));
            }
        }
    }
    candidates.sort_by_key(|item| item.0);
    candidates.pop().map(|item| item.1)
}

pub struct CliAnalysisProvider {
    config: AgentConfig,
}

impl CliAnalysisProvider {
    pub fn new(config: AgentConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl AnalysisProvider for CliAnalysisProvider {
    fn name(&self) -> &str {
        match self.config.agent {
            CliAgentKind::Codex => "codex-cli",
            CliAgentKind::Claude => "claude-cli",
        }
    }

    async fn analyze(&self, request: &AnalysisRequest) -> Result<AnalysisResponse, AnalysisError> {
        let prompt = analysis_prompt(request).map_err(|e| AnalysisError::Invalid(e.to_string()))?;
        let mut command = Command::new(&self.config.executable);
        match self.config.agent {
            CliAgentKind::Codex => {
                command.args([
                    "exec",
                    "--sandbox",
                    "read-only",
                    "--ephemeral",
                    "--skip-git-repo-check",
                    "--color",
                    "never",
                    "-",
                ]);
            }
            CliAgentKind::Claude => {
                command.args([
                    "--print",
                    "--tools",
                    "",
                    "--no-session-persistence",
                    "--output-format",
                    "text",
                    &prompt,
                ]);
            }
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| AnalysisError::Unavailable(e.to_string()))?;
        if self.config.agent == CliAgentKind::Codex
            && let Some(mut stdin) = child.stdin.take()
        {
            stdin
                .write_all(prompt.as_bytes())
                .await
                .map_err(|e| AnalysisError::Unavailable(e.to_string()))?;
        }
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| AnalysisError::Unavailable(e.to_string()))?;
        if !output.status.success() {
            return Err(AnalysisError::Unavailable(
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(1000)
                    .collect(),
            ));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let json = extract_json(&text)
            .ok_or_else(|| AnalysisError::Invalid("agent did not return a JSON object".into()))?;
        let response: AnalysisResponse =
            serde_json::from_str(json).map_err(|e| AnalysisError::Invalid(e.to_string()))?;
        validate(request, &response)?;
        Ok(response)
    }
}

fn analysis_prompt(request: &AnalysisRequest) -> Result<String, serde_json::Error> {
    Ok(format!(
        "You are the read-only diagnostic component of RescueLoop. Analyze the Incident IR below. Return ONLY one JSON object with exactly these fields: summary:string, hypotheses:[{{cause:string,confidence:number 0..1,evidence_indexes:[integer]}}], proposed_actions:[{{action_type:string,reason:string,parameters:object,reversible:boolean}}], needs_more_evidence:boolean. Allowed action_type values are: {}. Exact parameter schemas: quarantine_path={{\"target\":string}}, regenerate_cache={{\"target\":string}}, patch_json_config={{\"target\":string,\"pointer\":string,\"value\":any}}, set_permission={{\"target\":string,\"mode\":string}}, restart_service={{\"service_id\":string}}, restart_container={{\"engine\":\"docker\"|\"podman\",\"container_id\":string}}. Use only exact identities from evidence; never invent a path or ID. Do not emit shell commands. Refuse by setting needs_more_evidence=true and proposed_actions=[] when evidence is insufficient. Incident IR:\n{}",
        request.allowed_actions.join(", "),
        serde_json::to_string(request)?
    ))
}

fn extract_json(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    Some(&text[start..=end])
}

impl HttpAnalysisProvider {
    pub fn new(endpoint: impl Into<String>, bearer_token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: endpoint.into(),
            bearer_token,
        }
    }
}

#[async_trait]
impl AnalysisProvider for HttpAnalysisProvider {
    fn name(&self) -> &str {
        "http-json"
    }

    async fn analyze(&self, request: &AnalysisRequest) -> Result<AnalysisResponse, AnalysisError> {
        let mut call = self.client.post(&self.endpoint).json(request);
        if let Some(token) = &self.bearer_token {
            call = call.bearer_auth(token);
        }
        let response = call
            .send()
            .await
            .map_err(|e| AnalysisError::Unavailable(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(AnalysisError::Unavailable(format!("HTTP {status}")));
        }
        let analysis: AnalysisResponse = response
            .json()
            .await
            .map_err(|e| AnalysisError::Invalid(e.to_string()))?;
        validate(request, &analysis)?;
        Ok(analysis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rescueloop_core::{Evidence, Incident, IncidentKind, ProposedAction};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn request() -> AnalysisRequest {
        let incident = Incident::detected(
            "test",
            IncidentKind::Crash,
            "test",
            Evidence {
                source: "test".into(),
                summary: "test".into(),
                artifact: None,
                fields: BTreeMap::new(),
            },
        );
        AnalysisRequest::bounded(
            incident,
            ALLOWED_ACTIONS.iter().map(|x| x.to_string()).collect(),
        )
    }

    #[test]
    fn rejects_arbitrary_command() {
        let response = AnalysisResponse {
            summary: "x".into(),
            hypotheses: vec![],
            needs_more_evidence: false,
            proposed_actions: vec![ProposedAction {
                action_type: "run_shell".into(),
                reason: "x".into(),
                parameters: json!({"cmd":"rm"}),
                reversible: true,
            }],
        };
        assert!(validate(&request(), &response).is_err());
    }

    #[test]
    fn rejects_incomplete_typed_action() {
        let response = AnalysisResponse {
            summary: "x".into(),
            hypotheses: vec![],
            needs_more_evidence: false,
            proposed_actions: vec![ProposedAction {
                action_type: "quarantine_path".into(),
                reason: "x".into(),
                parameters: json!({}),
                reversible: true,
            }],
        };
        assert!(validate(&request(), &response).is_err());
    }

    #[test]
    fn rejects_invalid_typed_parameters() {
        let response = AnalysisResponse {
            summary: "x".into(),
            hypotheses: vec![],
            needs_more_evidence: false,
            proposed_actions: vec![ProposedAction {
                action_type: "restart_container".into(),
                reason: "x".into(),
                parameters: json!({"engine":"shell", "container_id":"abc"}),
                reversible: true,
            }],
        };
        assert!(validate(&request(), &response).is_err());
    }
}
