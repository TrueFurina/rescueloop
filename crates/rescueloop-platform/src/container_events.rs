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
    io::BufReader,
    process::{Child, ChildStdout, Command},
};

use crate::bounded_io::{self, Line};

const FAILURE_ACTIONS: &[&str] = &["die", "oom", "health_status: unhealthy"];
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_TRACKED_CONTAINERS: usize = 4_096;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

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
    events: Option<BufReader<ChildStdout>>,
    failures: HashMap<String, VecDeque<Instant>>,
}

impl ContainerEventSource {
    fn new(engine: &'static str) -> Self {
        Self {
            engine,
            process: None,
            events: None,
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
        self.events = Some(BufReader::new(stdout));
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
        let (sender, mut events) = tokio::sync::mpsc::channel(1);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if event.is_ok() {
                    let _ = sender.try_send(());
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
        let output = bounded_command(self.engine, &["inspect", "--", id])
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
        let Ok(output) = bounded_command(self.engine, &["logs", "--tail", "100", "--", id]).await
        else {
            return Vec::new();
        };
        diagnostic_log_lines(&[output.stdout, output.stderr].concat())
    }

    fn record_failure(&mut self, id: &str) -> bool {
        let now = Instant::now();
        for window in self.failures.values_mut() {
            prune_window(window, now);
        }
        self.failures.retain(|_, window| !window.is_empty());
        if self.failures.len() >= MAX_TRACKED_CONTAINERS
            && !self.failures.contains_key(id)
            && let Some(oldest) = self
                .failures
                .iter()
                .min_by_key(|(_, window)| window.back().copied())
                .map(|(id, _)| id.clone())
        {
            self.failures.remove(&oldest);
        }
        let window = self.failures.entry(id.to_string()).or_default();
        window.push_back(now);
        prune_window(window, now);
        window.len() >= 3
    }
}

fn prune_window(window: &mut VecDeque<Instant>, now: Instant) {
    while window
        .front()
        .is_some_and(|instant| now.duration_since(*instant) > Duration::from_secs(60))
    {
        window.pop_front();
    }
}

struct CommandOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn bounded_command(engine: &str, arguments: &[&str]) -> Result<CommandOutput> {
    bounded_command_with_timeout(engine, arguments, COMMAND_TIMEOUT).await
}

async fn bounded_command_with_timeout(
    engine: &str,
    arguments: &[&str],
    timeout: Duration,
) -> Result<CommandOutput> {
    let mut command = Command::new(engine);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .context("container command has no stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("container command has no stderr")?;
    let stdout = tokio::spawn(bounded_io::drain(stdout, MAX_COMMAND_OUTPUT_BYTES));
    let stderr = tokio::spawn(bounded_io::drain(stderr, MAX_COMMAND_OUTPUT_BYTES));
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            terminate_child(&mut child).await;
            let _ = child.wait().await;
            let _ = stdout.await;
            let _ = stderr.await;
            bail!("container command timed out")
        }
    };
    Ok(CommandOutput {
        status,
        stdout: stdout.await.context("container stdout reader stopped")??,
        stderr: stderr.await.context("container stderr reader stopped")??,
    })
}

async fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        return;
    }
    let _ = child.kill().await;
}

fn valid_container_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
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
        if self.events.is_none() {
            self.connect().await?;
        }
        loop {
            let line = bounded_io::read_line(
                self.events
                    .as_mut()
                    .context("container event source disconnected")?,
                MAX_EVENT_BYTES,
            )
            .await?;
            let line = match line {
                Line::Value(line) => line,
                Line::Oversized => continue,
                Line::End => {
                    self.events = None;
                    self.process = None;
                    bail!("{} event stream closed", self.engine)
                }
            };
            let Ok(event) = serde_json::from_slice::<EngineEvent>(&line) else {
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
            if !valid_container_id(id) {
                continue;
            }
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
    crate::diagnostics::select_lines(&String::from_utf8_lossy(bytes), KEYS, &[], 30)
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

    #[test]
    fn bounds_restart_history_and_rejects_option_injection() {
        let mut source = ContainerEventSource::new("docker");
        for index in 0..MAX_TRACKED_CONTAINERS + 100 {
            source.record_failure(&format!("container{index}"));
        }
        assert_eq!(source.failures.len(), MAX_TRACKED_CONTAINERS);
        assert!(valid_container_id("a1B2c3"));
        assert!(!valid_container_id("--all"));
        assert!(!valid_container_id(""));
    }

    #[tokio::test]
    async fn bounds_command_output_while_draining_the_stream() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let write = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            writer.write_all(&vec![b'x'; 32 * 1024]).await.unwrap();
        });
        let retained = bounded_io::drain(reader, 4096).await.unwrap();
        write.await.unwrap();
        assert_eq!(retained.len(), 4096);
    }

    #[tokio::test]
    async fn discards_oversized_event_and_resynchronizes_at_newline() {
        let (mut writer, reader) = tokio::io::duplex(MAX_EVENT_BYTES * 2);
        let write = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            writer
                .write_all(&vec![b'x'; MAX_EVENT_BYTES + 1])
                .await
                .unwrap();
            writer.write_all(b"\n{}\n").await.unwrap();
        });
        let mut reader = BufReader::new(reader);
        assert!(matches!(
            bounded_io::read_line(&mut reader, MAX_EVENT_BYTES)
                .await
                .unwrap(),
            Line::Oversized
        ));
        assert!(matches!(
            bounded_io::read_line(&mut reader, MAX_EVENT_BYTES).await.unwrap(),
            Line::Value(line) if line == b"{}"
        ));
        write.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kills_a_hung_container_command_at_the_deadline() {
        use std::os::unix::fs::PermissionsExt;

        let script = std::env::temp_dir().join(format!("rescueloop-hung-{}", uuid::Uuid::new_v4()));
        std::fs::write(&script, b"#!/bin/sh\nsleep 10\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let started = Instant::now();
        let result =
            bounded_command_with_timeout(script.to_str().unwrap(), &[], Duration::from_millis(50))
                .await;
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        std::fs::remove_file(script).unwrap();
    }
}
