use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use rescueloop_core::{Evidence, Incident, IncidentCollector, IncidentKind};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};
use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};

pub struct ArtifactWatcher {
    name: &'static str,
    platform: &'static str,
    extensions: HashSet<String>,
    seen: HashSet<PathBuf>,
    events: mpsc::UnboundedReceiver<PathBuf>,
    _watcher: RecommendedWatcher,
}

impl ArtifactWatcher {
    pub fn new(
        name: &'static str,
        platform: &'static str,
        roots: Vec<PathBuf>,
        extensions: &[&str],
    ) -> Result<Self> {
        let (sender, events) = mpsc::unbounded_channel();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if let Ok(event) = event {
                    for path in event.paths {
                        let _ = sender.send(path);
                    }
                }
            })
            .context("failed to initialize native filesystem event watcher")?;
        let mut watched = 0;
        for root in roots {
            if root.is_dir() && watcher.watch(&root, RecursiveMode::Recursive).is_ok() {
                watched += 1;
            }
        }
        if watched == 0 {
            bail!("no diagnostic report directories are available to watch")
        }
        Ok(Self {
            name,
            platform,
            extensions: extensions.iter().map(|x| x.to_string()).collect(),
            seen: HashSet::new(),
            events,
            _watcher: watcher,
        })
    }
    fn to_incident(&self, path: PathBuf) -> Incident {
        let metadata = std::fs::metadata(&path).ok();
        let bytes = std::fs::read(&path).unwrap_or_default();
        let mut fields = BTreeMap::new();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        fields.insert("sha256".into(), serde_json::json!(digest));
        fields.insert(
            "size_bytes".into(),
            serde_json::json!(metadata.map(|m| m.len()).unwrap_or_default()),
        );
        fields.insert(
            "diagnostic_lines".into(),
            serde_json::json!(diagnostic_lines(&bytes)),
        );
        let filename = path
            .file_name()
            .and_then(|x| x.to_str())
            .unwrap_or("unknown report")
            .to_owned();
        let kind = match path.extension().and_then(|x| x.to_str()) {
            Some("spin" | "hang") => IncidentKind::Hang,
            _ => IncidentKind::Crash,
        };
        let mut incident = Incident::detected(
            self.platform,
            kind,
            format!("Detected failure report: {filename}"),
            Evidence {
                source: self.name.into(),
                summary: "The operating system created a diagnostic failure artifact".into(),
                artifact: Some(path),
                fields,
            },
        );
        incident.application = report_application(&bytes)
            .or_else(|| filename.split(['_', '-']).next().map(str::to_string));
        incident.id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, bytes.as_slice());
        incident
    }
}

fn report_application(bytes: &[u8]) -> Option<String> {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
        for key in ["procName", "app_name", "applicationName"] {
            if let Some(name) = value.get(key).and_then(|value| value.as_str()) {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn is_rescueloop(name: Option<&str>) -> bool {
    name.is_some_and(|name| name.to_ascii_lowercase().starts_with("rescueloop"))
}

/// Keeps diagnostics while excluding paths and commands.
fn diagnostic_lines(bytes: &[u8]) -> Vec<String> {
    const KEYS: &[&str] = &[
        "app_name",
        "applicationname",
        "bug_type",
        "exception",
        "faulting",
        "exceptioncode",
        "exceptiontype",
        "os_version",
        "problem signature",
        "termination",
        "version",
    ];
    crate::diagnostics::select_lines(
        &String::from_utf8_lossy(bytes),
        KEYS,
        &["path", "command"],
        40,
    )
}

#[async_trait]
impl IncidentCollector for ArtifactWatcher {
    fn name(&self) -> &str {
        self.name
    }
    async fn next_incident(&mut self) -> Result<Incident> {
        loop {
            let path = self
                .events
                .recv()
                .await
                .context("native filesystem event stream closed")?;
            let supported = path
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| self.extensions.contains(&x.to_ascii_lowercase()));
            if !supported || self.seen.contains(&path) || !path.is_file() {
                continue;
            }
            sleep(Duration::from_millis(350)).await;
            let first_size = std::fs::metadata(&path)
                .map(|value| value.len())
                .unwrap_or(0);
            sleep(Duration::from_millis(150)).await;
            let second_size = std::fs::metadata(&path)
                .map(|value| value.len())
                .unwrap_or(0);
            if first_size == 0 || first_size != second_size {
                continue;
            }
            self.seen.insert(path.clone());
            let incident = self.to_incident(path);
            if is_rescueloop(incident.application.as_deref()) {
                continue;
            }
            return Ok(incident);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{diagnostic_lines, is_rescueloop, report_application};

    #[test]
    fn extracts_diagnostics_but_not_paths_or_commands() {
        let report = b"App_Name=Demo\nExceptionCode=c0000005\nCommandLine=secret\nPath=/Users/alice/private\n";
        let result = diagnostic_lines(report);
        assert_eq!(result, vec!["App_Name=Demo", "ExceptionCode=c0000005"]);
    }

    #[test]
    fn identifies_and_excludes_own_crash_reports() {
        assert_eq!(
            report_application(br#"{"procName":"RescueLoop"}"#).as_deref(),
            Some("RescueLoop")
        );
        assert!(is_rescueloop(Some("rescueloop-crash-demo")));
        assert!(!is_rescueloop(Some("demo-app")));
    }
}
