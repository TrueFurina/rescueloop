use anyhow::{Context, Result, bail};
use async_trait::async_trait;
#[cfg(unix)]
use notify::{RecursiveMode, Watcher};
use rescueloop_core::{Confidence, Evidence, Incident, IncidentCollector, IncidentKind};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader, Lines},
    process::{Child, ChildStdout, Command},
};

const FAILURE_ACTIONS: &[&str] = &["die", "oom", "health_status: unhealthy"];

pub fn available_sources() -> Vec<Box<dyn IncidentCollector>> {
    ["docker", "podman"]
        .into_iter()
        .filter(|engine| executable_exists(engine))
        .map(|engine| Box::new(ContainerEventSource::new(engine)) as Box<dyn IncidentCollector>)
        .collect()
}

fn executable_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|path| {
            let candidate = path.join(if cfg!(windows) {
                format!("{name}.exe")
            } else {
                name.to_string()
            });
            candidate.is_file()
        })
    })
}

pub struct ContainerEventSource {
    engine: &'static str,
    process: Option<Child>,
    lines: Option<Lines<BufReader<ChildStdout>>>,
    failures: HashMap<String, VecDeque<Instant>>,
}

impl ContainerEventSource {
    fn new(engine: &'static str) -> Self {
        Self {
            engine,
            process: None,
            lines: None,
            failures: HashMap::new(),
        }
    }

    async fn connect(&mut self) -> Result<()> {
        self.wait_for_engine_socket().await?;
        let mut child = Command::new(self.engine)
            .args([
                "events",
                "--filter",
                "type=container",
                "--format",
                "{{json .}}",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to subscribe to {} events", self.engine))?;
        let stdout = child
            .stdout
            .take()
            .context("container event stream has no stdout")?;
        self.lines = Some(BufReader::new(stdout).lines());
        self.process = Some(child);
        Ok(())
    }

    #[cfg(unix)]
    async fn wait_for_engine_socket(&self) -> Result<()> {
        if std::env::var_os("DOCKER_HOST").is_some() || self.engine != "docker" {
            return Ok(());
        }
        let mut candidates = vec![PathBuf::from("/var/run/docker.sock")];
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            candidates.push(home.join(".docker/run/docker.sock"));
            candidates.push(home.join(".colima/default/docker.sock"));
            candidates.push(home.join(".colima/docker.sock"));
        }
        if candidates.iter().any(|path| path.exists()) {
            return Ok(());
        }
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if event.is_ok() {
                    let _ = sender.send(());
                }
            })?;
        for parent in candidates.iter().filter_map(|path| path.parent()) {
            if parent.exists() {
                let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
            }
        }
        while events.recv().await.is_some() {
            if candidates.iter().any(|path| path.exists()) {
                return Ok(());
            }
        }
        bail!("Docker socket watcher closed")
    }

    #[cfg(not(unix))]
    async fn wait_for_engine_socket(&self) -> Result<()> {
        Ok(())
    }

    async fn inspect(&self, id: &str) -> Option<Value> {
        let output = Command::new(self.engine)
            .args(["inspect", id])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        serde_json::from_slice::<Vec<Value>>(&output.stdout)
            .ok()?
            .into_iter()
            .next()
    }

    async fn diagnostic_logs(&self, id: &str) -> Vec<String> {
        let Ok(output) = Command::new(self.engine)
            .args(["logs", "--tail", "100", id])
            .stdin(Stdio::null())
            .output()
            .await
        else {
            return Vec::new();
        };
        diagnostic_log_lines(&[output.stdout, output.stderr].concat())
    }

    fn record_failure(&mut self, id: &str) -> bool {
        let now = Instant::now();
        let window = self.failures.entry(id.to_string()).or_default();
        window.push_back(now);
        while window
            .front()
            .is_some_and(|instant| now.duration_since(*instant) > Duration::from_secs(60))
        {
            window.pop_front();
        }
        window.len() >= 3
    }
}

#[derive(Debug, Deserialize)]
struct EngineEvent {
    #[serde(rename = "Action", alias = "status")]
    action: String,
    #[serde(rename = "Actor", default)]
    actor: Actor,
    #[serde(rename = "id", default)]
    id: String,
}

#[derive(Debug, Default, Deserialize)]
struct Actor {
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(rename = "Attributes", default)]
    attributes: BTreeMap<String, String>,
}

#[async_trait]
impl IncidentCollector for ContainerEventSource {
    fn name(&self) -> &str {
        self.engine
    }

    async fn next_incident(&mut self) -> Result<Incident> {
        if self.lines.is_none() {
            self.connect().await?;
        }
        loop {
            let line = self
                .lines
                .as_mut()
                .context("container event source disconnected")?
                .next_line()
                .await?;
            let Some(line) = line else {
                self.lines = None;
                self.process = None;
                bail!("{} event stream closed", self.engine)
            };
            let Ok(event) = serde_json::from_str::<EngineEvent>(&line) else {
                continue;
            };
            if !FAILURE_ACTIONS.contains(&event.action.as_str()) {
                continue;
            }
            let id = if event.actor.id.is_empty() {
                event.id.as_str()
            } else {
                event.actor.id.as_str()
            };
            let restart_loop = self.record_failure(id);
            let inspect = self.inspect(id).await;
            let logs = self.diagnostic_logs(id).await;
            return Ok(normalize_event(
                self.engine,
                event,
                inspect,
                restart_loop,
                logs,
            ));
        }
    }
}

fn normalize_event(
    engine: &str,
    event: EngineEvent,
    inspect: Option<Value>,
    restart_loop: bool,
    diagnostic_logs: Vec<String>,
) -> Incident {
    let container_id = if event.actor.id.is_empty() {
        event.id.clone()
    } else {
        event.actor.id.clone()
    };
    let name = event
        .actor
        .attributes
        .get("name")
        .cloned()
        .unwrap_or_else(|| container_id.chars().take(12).collect());
    let state = inspect.as_ref().and_then(|value| value.get("State"));
    let oom = event.action == "oom"
        || state
            .and_then(|value| value.get("OOMKilled"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let unhealthy = event.action.contains("unhealthy");
    let kind = if oom {
        IncidentKind::OutOfMemory
    } else if unhealthy {
        IncidentKind::Unhealthy
    } else if restart_loop {
        IncidentKind::RestartLoop
    } else {
        IncidentKind::ContainerExit
    };
    let exit_code = state
        .and_then(|value| value.get("ExitCode"))
        .and_then(Value::as_i64);
    let mut fields = BTreeMap::new();
    fields.insert("engine".into(), Value::String(engine.into()));
    fields.insert("container_id".into(), Value::String(container_id));
    fields.insert("event".into(), Value::String(event.action.clone()));
    fields.insert("exit_code".into(), serde_json::json!(exit_code));
    fields.insert("oom_killed".into(), Value::Bool(oom));
    fields.insert("restart_loop".into(), Value::Bool(restart_loop));
    fields.insert(
        "diagnostic_output".into(),
        serde_json::json!(diagnostic_logs),
    );
    if let Some(error) = state
        .and_then(|value| value.get("Error"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        fields.insert("engine_error".into(), Value::String(error.into()));
    }
    let mut incident = Incident::detected(
        std::env::consts::OS,
        kind,
        format!("{engine} container {name} emitted {}", event.action),
        Evidence {
            source: format!("{engine}-events"),
            summary: "Container engine emitted an objective failure event".into(),
            artifact: None::<PathBuf>,
            fields,
        },
    );
    incident.application = Some(name.clone());
    incident.application_identity = Some(rescueloop_core::ApplicationIdentity {
        name,
        runtime: Some(engine.into()),
        ..Default::default()
    });
    incident.normalized_failure.code = exit_code.map(|code| format!("exit:{code}"));
    incident.normalized_failure.resource_bucket = oom.then(|| "memory".into());
    incident.confidence = Confidence::Confirmed;
    incident
}

fn diagnostic_log_lines(bytes: &[u8]) -> Vec<String> {
    const KEYS: &[&str] = &[
        "error",
        "exception",
        "fatal",
        "fail",
        "panic",
        "oom",
        "killed",
        "unhealthy",
        "refused",
        "timeout",
    ];
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            KEYS.iter().any(|key| lower.contains(key))
        })
        .take(30)
        .map(|line| line.chars().take(500).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_oom_from_inspect_state() {
        let event = EngineEvent {
            action: "die".into(),
            actor: Actor {
                id: "abc".into(),
                attributes: BTreeMap::from([("name".into(), "api".into())]),
            },
            id: String::new(),
        };
        let incident = normalize_event(
            "docker",
            event,
            Some(serde_json::json!({"State":{"OOMKilled":true,"ExitCode":137}})),
            false,
            vec!["fatal: allocation failed".into()],
        );
        assert_eq!(incident.kind, IncidentKind::OutOfMemory);
        assert_eq!(incident.application.as_deref(), Some("api"));
        assert_eq!(
            incident.normalized_failure.code.as_deref(),
            Some("exit:137")
        );
    }

    #[test]
    fn retains_only_diagnostic_container_logs() {
        let lines = diagnostic_log_lines(b"ready\nconnection refused\nrequest complete\n");
        assert_eq!(lines, vec!["connection refused"]);
    }
}
