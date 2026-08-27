use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use rescueloop_core::{Evidence, Incident, IncidentCollector, IncidentKind};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fs::File,
    io::{BufReader, Read},
    path::{Path, PathBuf},
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
    seen_order: VecDeque<PathBuf>,
    events: mpsc::UnboundedReceiver<PathBuf>,
    _watcher: RecommendedWatcher,
}

const MAX_SEEN_PATHS: usize = 4_096;
const MAX_DIAGNOSTIC_BYTES: usize = 1024 * 1024;

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
            seen_order: VecDeque::new(),
            events,
            _watcher: watcher,
        })
    }
    fn remember(&mut self, path: PathBuf) {
        if self.seen.insert(path.clone()) {
            self.seen_order.push_back(path);
        }
        while self.seen_order.len() > MAX_SEEN_PATHS {
            if let Some(expired) = self.seen_order.pop_front() {
                self.seen.remove(&expired);
            }
        }
    }

    async fn to_incident(&self, path: PathBuf) -> Result<Incident> {
        let name = self.name;
        let platform = self.platform;
        tokio::task::spawn_blocking(move || build_incident(name, platform, path))
            .await
            .context("diagnostic artifact worker stopped")?
    }
}

fn build_incident(name: &'static str, platform: &'static str, path: PathBuf) -> Result<Incident> {
    let (bytes, digest, size_bytes) = read_report(&path)?;
    let mut fields = BTreeMap::new();
    fields.insert("sha256".into(), serde_json::json!(digest));
    fields.insert("size_bytes".into(), serde_json::json!(size_bytes));
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
        platform,
        kind,
        format!("Detected failure report: {filename}"),
        Evidence {
            source: name.into(),
            summary: "The operating system created a diagnostic failure artifact".into(),
            artifact: Some(path),
            fields,
        },
    );
    incident.application = report_application(&bytes)
        .or_else(|| filename.split(['_', '-']).next().map(str::to_string));
    incident.id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, digest.as_bytes());
    Ok(incident)
}

fn read_report(path: &Path) -> Result<(Vec<u8>, String, u64)> {
    let file =
        File::open(path).with_context(|| format!("cannot open report: {}", path.display()))?;
    let size_bytes = file.metadata().map(|value| value.len()).unwrap_or_default();
    let mut reader = BufReader::new(file);
    let mut diagnostic_bytes = Vec::with_capacity(
        usize::try_from(size_bytes)
            .unwrap_or(MAX_DIAGNOSTIC_BYTES)
            .min(MAX_DIAGNOSTIC_BYTES),
    );
    let mut digest = Sha256::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        digest.update(&chunk[..read]);
        let remaining = MAX_DIAGNOSTIC_BYTES.saturating_sub(diagnostic_bytes.len());
        diagnostic_bytes.extend_from_slice(&chunk[..read.min(remaining)]);
    }
    Ok((
        diagnostic_bytes,
        format!("{:x}", digest.finalize()),
        size_bytes,
    ))
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
            self.remember(path.clone());
            let incident = self.to_incident(path).await?;
            if is_rescueloop(incident.application.as_deref()) {
                continue;
            }
            return Ok(incident);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ArtifactWatcher, MAX_DIAGNOSTIC_BYTES, MAX_SEEN_PATHS, diagnostic_lines, is_rescueloop,
        read_report, report_application,
    };
    use sha2::Digest;
    use std::{collections::HashSet, fs};

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

    #[test]
    fn bounds_artifact_memory_but_hashes_the_complete_file() {
        let path = std::env::temp_dir().join(format!("rescueloop-report-{}", uuid::Uuid::new_v4()));
        let content = vec![b'x'; MAX_DIAGNOSTIC_BYTES + 64 * 1024];
        fs::write(&path, &content).unwrap();
        let (retained, digest, size) = read_report(&path).unwrap();
        assert_eq!(retained.len(), MAX_DIAGNOSTIC_BYTES);
        assert_eq!(size, content.len() as u64);
        assert_eq!(digest, format!("{:x}", sha2::Sha256::digest(&content)));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn bounds_seen_path_cache() {
        let (_sender, events) = tokio::sync::mpsc::unbounded_channel();
        let root = std::env::temp_dir();
        let watcher = notify::recommended_watcher(|_| {}).unwrap();
        let mut source = ArtifactWatcher {
            name: "test",
            platform: "test",
            extensions: HashSet::new(),
            seen: HashSet::new(),
            seen_order: Default::default(),
            events,
            _watcher: watcher,
        };
        for index in 0..MAX_SEEN_PATHS + 10 {
            source.remember(root.join(index.to_string()));
        }
        assert_eq!(source.seen.len(), MAX_SEEN_PATHS);
        assert!(!source.seen.contains(&root.join("0")));
    }
}
