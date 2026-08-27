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
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
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
    roots: Vec<PathBuf>,
    events: mpsc::Receiver<PathBuf>,
    overflowed: Arc<AtomicBool>,
    reconciliation: Option<mpsc::Receiver<PathBuf>>,
    _watcher: RecommendedWatcher,
}

const MAX_SEEN_PATHS: usize = 4_096;
const MAX_DIAGNOSTIC_BYTES: usize = 1024 * 1024;
const EVENT_QUEUE_CAPACITY: usize = 1_024;
const RECONCILIATION_CAPACITY: usize = 256;

impl ArtifactWatcher {
    pub fn new(
        name: &'static str,
        platform: &'static str,
        roots: Vec<PathBuf>,
        extensions: &[&str],
    ) -> Result<Self> {
        let (sender, events) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        let overflowed = Arc::new(AtomicBool::new(false));
        let callback_overflowed = Arc::clone(&overflowed);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
                Ok(event) => {
                    for path in event.paths {
                        if sender.try_send(path).is_err() {
                            callback_overflowed.store(true, Ordering::Release);
                        }
                    }
                }
                Err(_) => callback_overflowed.store(true, Ordering::Release),
            })
            .context("failed to initialize native filesystem event watcher")?;
        let mut watched = 0;
        for root in &roots {
            if root.is_dir() && watcher.watch(root, RecursiveMode::Recursive).is_ok() {
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
            roots,
            events,
            overflowed,
            reconciliation: None,
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

    fn start_reconciliation(&mut self) {
        let roots = self.roots.clone();
        let extensions = self.extensions.clone();
        let seen = self.seen.clone();
        let (sender, receiver) = mpsc::channel(RECONCILIATION_CAPACITY);
        self.reconciliation = Some(receiver);
        tokio::task::spawn_blocking(move || scan_reports(&roots, &extensions, &seen, sender));
    }

    async fn next_path(&mut self) -> Result<PathBuf> {
        loop {
            if let Some(reconciliation) = &mut self.reconciliation {
                if let Some(path) = reconciliation.recv().await {
                    return Ok(path);
                }
                self.reconciliation = None;
                continue;
            }
            if self.overflowed.swap(false, Ordering::AcqRel) {
                tracing::warn!(
                    event = "source.overflow_reconciling",
                    source = self.name,
                    "Native artifact event queue overflowed; reconciling watched roots"
                );
                self.start_reconciliation();
                continue;
            }
            return self
                .events
                .recv()
                .await
                .context("native filesystem event stream closed");
        }
    }
}

fn scan_reports(
    roots: &[PathBuf],
    extensions: &HashSet<String>,
    seen: &HashSet<PathBuf>,
    sender: mpsc::Sender<PathBuf>,
) {
    let mut pending = roots.to_vec();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() && !file_type.is_symlink() {
                pending.push(path);
                continue;
            }
            let supported = path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| extensions.contains(&value.to_ascii_lowercase()));
            if supported && !seen.contains(&path) && sender.blocking_send(path).is_err() {
                return;
            }
        }
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
    let initial = file.metadata()?;
    let size_bytes = initial.len();
    let initial_modified = initial.modified().ok();
    let mut reader = BufReader::new(file);
    let mut diagnostic_bytes = Vec::with_capacity(
        usize::try_from(size_bytes)
            .unwrap_or(MAX_DIAGNOSTIC_BYTES)
            .min(MAX_DIAGNOSTIC_BYTES),
    );
    let mut digest = Sha256::new();
    let mut chunk = [0_u8; 64 * 1024];
    let mut bytes_read = 0_u64;
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        bytes_read = bytes_read.saturating_add(read as u64);
        digest.update(&chunk[..read]);
        let remaining = MAX_DIAGNOSTIC_BYTES.saturating_sub(diagnostic_bytes.len());
        diagnostic_bytes.extend_from_slice(&chunk[..read.min(remaining)]);
    }
    let final_metadata = reader.get_ref().metadata()?;
    if bytes_read != size_bytes
        || final_metadata.len() != size_bytes
        || final_metadata.modified().ok() != initial_modified
    {
        anyhow::bail!("diagnostic artifact changed while it was being read")
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
            let path = self.next_path().await?;
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
            let incident = match self.to_incident(path.clone()).await {
                Ok(incident) => incident,
                Err(error) => {
                    tracing::warn!(
                        event = "source.artifact_unreadable",
                        source = self.name,
                        error = %error,
                        "Diagnostic artifact could not be normalized"
                    );
                    continue;
                }
            };
            self.remember(path);
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
        let (_sender, events) = tokio::sync::mpsc::channel(1);
        let root = std::env::temp_dir();
        let watcher = notify::recommended_watcher(|_| {}).unwrap();
        let mut source = ArtifactWatcher {
            name: "test",
            platform: "test",
            extensions: HashSet::new(),
            seen: HashSet::new(),
            seen_order: Default::default(),
            roots: Vec::new(),
            events,
            overflowed: Default::default(),
            reconciliation: None,
            _watcher: watcher,
        };
        for index in 0..MAX_SEEN_PATHS + 10 {
            source.remember(root.join(index.to_string()));
        }
        assert_eq!(source.seen.len(), MAX_SEEN_PATHS);
        assert!(!source.seen.contains(&root.join("0")));
    }

    #[tokio::test]
    async fn overflow_reconciliation_streams_unseen_reports() {
        let root = std::env::temp_dir().join(format!("rescueloop-scan-{}", uuid::Uuid::new_v4()));
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let report = nested.join("demo.crash");
        fs::write(&report, b"Exception: boom").unwrap();
        fs::write(nested.join("ignored.txt"), b"ignored").unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let roots = vec![root.clone()];
        let extensions = HashSet::from(["crash".to_string()]);

        tokio::task::spawn_blocking(move || {
            super::scan_reports(&roots, &extensions, &HashSet::new(), sender)
        })
        .await
        .unwrap();
        assert_eq!(receiver.recv().await, Some(report));
        assert!(receiver.recv().await.is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
