use rescueloop_core::IncidentCollector;

pub fn available_sources() -> Vec<Box<dyn IncidentCollector>> {
    #[cfg(target_os = "macos")]
    return if unsafe { libc::geteuid() } == 0 {
        vec![Box::new(macos::MacOsLogSource::default())]
    } else {
        Vec::new()
    };
    #[cfg(target_os = "windows")]
    return vec![Box::new(windows::WindowsEventSource::default())];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Vec::new()
}

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{Context, Result, bail};
    use async_trait::async_trait;
    use rescueloop_core::{Confidence, Evidence, Incident, IncidentCollector, IncidentKind};
    use serde_json::Value;
    use std::{collections::BTreeMap, process::Stdio};
    use tokio::{
        io::BufReader,
        process::{Child, ChildStdout, Command},
    };

    use crate::bounded_io::{self, Line};

    const MAX_EVENT_BYTES: usize = 64 * 1024;

    #[derive(Default)]
    pub struct MacOsLogSource {
        child: Option<Child>,
        events: Option<BufReader<ChildStdout>>,
    }

    impl MacOsLogSource {
        async fn connect(&mut self) -> Result<()> {
            let predicate = r#"(process == "launchd" OR process == "runningboardd" OR process == "kernel") AND (eventMessage CONTAINS[c] "exited" OR eventMessage CONTAINS[c] "watchdog" OR eventMessage CONTAINS[c] "jetsam" OR eventMessage CONTAINS[c] "out of memory" OR eventMessage CONTAINS[c] "unhealthy")"#;
            let mut child = Command::new("/usr/bin/log")
                .args([
                    "stream",
                    "--style",
                    "ndjson",
                    "--level",
                    "error",
                    "--predicate",
                    predicate,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .context("failed to subscribe to macOS Unified Log")?;
            let stdout = child
                .stdout
                .take()
                .context("Unified Log stream has no stdout")?;
            self.events = Some(BufReader::new(stdout));
            self.child = Some(child);
            Ok(())
        }
    }

    fn service_identity(message: &str, fallback: &str) -> String {
        if let Some(rest) = message.split("service<").nth(1)
            && let Some(value) = rest.split('>').next()
        {
            let value = value.split('(').next().unwrap_or(value).trim();
            if !value.is_empty()
                && value.chars().all(|character| {
                    character.is_ascii_alphanumeric() || ".-_/@".contains(character)
                })
            {
                return value.to_string();
            }
        }
        fallback.to_string()
    }

    #[async_trait]
    impl IncidentCollector for MacOsLogSource {
        fn name(&self) -> &str {
            "macos-unified-log"
        }

        async fn next_incident(&mut self) -> Result<Incident> {
            if self.events.is_none() {
                self.connect().await?;
            }
            loop {
                let line = bounded_io::read_line(
                    self.events.as_mut().context("Unified Log disconnected")?,
                    MAX_EVENT_BYTES,
                )
                .await?;
                let line = match line {
                    Line::Value(line) => line,
                    Line::Oversized => continue,
                    Line::End => {
                        self.events = None;
                        self.child = None;
                        bail!("macOS Unified Log stream closed")
                    }
                };
                let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                let message = value
                    .get("eventMessage")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if message.is_empty() {
                    continue;
                }
                let lower = message.to_ascii_lowercase();
                let kind = if lower.contains("jetsam") || lower.contains("out of memory") {
                    IncidentKind::OutOfMemory
                } else if lower.contains("watchdog") {
                    IncidentKind::Hang
                } else {
                    IncidentKind::ServiceFailure
                };
                let producer = value
                    .get("senderImagePath")
                    .and_then(Value::as_str)
                    .and_then(|path| path.rsplit('/').next())
                    .or_else(|| {
                        value
                            .get("processImagePath")
                            .and_then(Value::as_str)
                            .and_then(|path| path.rsplit('/').next())
                    })
                    .or_else(|| value.get("process").and_then(Value::as_str))
                    .unwrap_or("system-service");
                let service_id = service_identity(message, producer);
                if service_id.to_ascii_lowercase().starts_with("rescueloop") {
                    continue;
                }
                let mut fields = BTreeMap::new();
                fields.insert("process".into(), Value::String(producer.into()));
                fields.insert("service_id".into(), Value::String(service_id.clone()));
                fields.insert(
                    "diagnostic_output".into(),
                    serde_json::json!([message.chars().take(1000).collect::<String>()]),
                );
                let mut incident = Incident::detected(
                    "macos",
                    kind,
                    format!("System reported a failure for {service_id}"),
                    Evidence {
                        source: "macos-unified-log".into(),
                        summary: "macOS emitted an objective service or resource failure".into(),
                        artifact: None,
                        fields,
                    },
                );
                incident.application = Some(service_id);
                incident.confidence = Confidence::Probable;
                return Ok(incident);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::service_identity;

        #[test]
        fn extracts_launchd_service_label() {
            assert_eq!(
                service_identity(
                    "service<com.example.worker(501)> exited with status 1",
                    "launchd"
                ),
                "com.example.worker"
            );
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use anyhow::{Context, Result, bail};
    use async_trait::async_trait;
    use rescueloop_core::{Confidence, Evidence, Incident, IncidentCollector, IncidentKind};
    use serde::Deserialize;
    use serde_json::Value;
    use std::{collections::BTreeMap, process::Stdio};
    use tokio::{
        io::BufReader,
        process::{Child, ChildStdout, Command},
    };

    use crate::bounded_io::{self, Line};

    const MAX_EVENT_BYTES: usize = 64 * 1024;

    #[derive(Default)]
    pub struct WindowsEventSource {
        child: Option<Child>,
        events: Option<BufReader<ChildStdout>>,
    }
    #[derive(Deserialize)]
    struct Event {
        provider: String,
        id: u32,
        message: String,
        #[serde(default)]
        service_id: Option<String>,
    }

    impl WindowsEventSource {
        async fn connect(&mut self) -> Result<()> {
            let script = r#"$q=[System.Diagnostics.Eventing.Reader.EventLogQuery]::new('System',[System.Diagnostics.Eventing.Reader.PathType]::LogName,'*[System[(Level=1 or Level=2)]]');$w=[System.Diagnostics.Eventing.Reader.EventLogWatcher]::new($q);Register-ObjectEvent $w EventRecordWritten -Action {$r=$Event.SourceEventArgs.EventRecord;if($r){$sid=$null;try{$x=[xml]$r.ToXml();$d=@($x.Event.EventData.Data);$named=$d|Where-Object{$_.Name -match 'ServiceName|param1'}|Select-Object -First 1;if($named){$sid=[string]$named.'#text'}elseif($d.Count -gt 0){$sid=[string]$d[0].'#text'}}catch{};@{provider=$r.ProviderName;id=$r.Id;message=$r.FormatDescription();service_id=$sid}|ConvertTo-Json -Compress}}|Out-Null;$w.Enabled=$true;while($true){Wait-Event|Remove-Event}"#;
            let mut child = Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", script])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .context("failed to subscribe to Windows Event Log")?;
            self.events = Some(BufReader::new(
                child
                    .stdout
                    .take()
                    .context("Windows event stream has no stdout")?,
            ));
            self.child = Some(child);
            Ok(())
        }
    }
    #[async_trait]
    impl IncidentCollector for WindowsEventSource {
        fn name(&self) -> &str {
            "windows-event-log"
        }
        async fn next_incident(&mut self) -> Result<Incident> {
            if self.events.is_none() {
                self.connect().await?;
            }
            loop {
                let line = bounded_io::read_line(
                    self.events
                        .as_mut()
                        .context("Windows Event Log disconnected")?,
                    MAX_EVENT_BYTES,
                )
                .await?;
                let line = match line {
                    Line::Value(line) => line,
                    Line::Oversized => continue,
                    Line::End => {
                        self.events = None;
                        self.child = None;
                        bail!("Windows Event Log stream closed")
                    }
                };
                let Ok(event) = serde_json::from_slice::<Event>(&line) else {
                    continue;
                };
                let lower = event.message.to_ascii_lowercase();
                let kind =
                    if lower.contains("out of memory") || lower.contains("resource exhaustion") {
                        IncidentKind::OutOfMemory
                    } else {
                        IncidentKind::ServiceFailure
                    };
                let mut fields = BTreeMap::new();
                fields.insert("provider".into(), Value::String(event.provider.clone()));
                fields.insert("event_id".into(), serde_json::json!(event.id));
                if let Some(service_id) = &event.service_id {
                    fields.insert("service_id".into(), Value::String(service_id.clone()));
                }
                fields.insert(
                    "diagnostic_output".into(),
                    serde_json::json!([event.message.chars().take(1000).collect::<String>()]),
                );
                let mut incident = Incident::detected(
                    "windows",
                    kind,
                    format!("Windows reported service failure {}", event.id),
                    Evidence {
                        source: "windows-event-log".into(),
                        summary: "Windows Event Log emitted a critical or error event".into(),
                        artifact: None,
                        fields,
                    },
                );
                incident.application = event.service_id.or(Some(event.provider));
                incident.confidence = Confidence::Probable;
                return Ok(incident);
            }
        }
    }
}
