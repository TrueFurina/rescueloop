mod artifact_watcher;
mod bounded_io;
mod container_events;
mod diagnostics;
mod os_events;
mod supervised;

use anyhow::Result;
use rescueloop_core::IncidentCollector;
use std::path::PathBuf;

pub use supervised::{ReplayResult, supervise, supervise_quiet, verify_replay};

pub fn event_sources(enabled: &[String]) -> Result<Vec<Box<dyn IncidentCollector>>> {
    let has = |name: &str| enabled.iter().any(|value| value == name);
    let mut sources = Vec::new();
    if has("system-artifacts") {
        sources.push(system_collector()?);
    }
    if has("containers") {
        sources.extend(container_events::available_sources());
    }
    if has("os-log") {
        sources.extend(os_events::available_sources());
    }
    if sources.is_empty() {
        anyhow::bail!("no available event sources are enabled")
    }
    Ok(sources)
}

pub fn system_collector() -> Result<Box<dyn IncidentCollector>> {
    #[cfg(target_os = "macos")]
    return Ok(Box::new(artifact_watcher::ArtifactWatcher::new(
        "macos-diagnostic-reports",
        "macos",
        macos_report_dirs(),
        &["crash", "ips", "diag", "spin", "hang"],
    )?));
    #[cfg(target_os = "windows")]
    return Ok(Box::new(artifact_watcher::ArtifactWatcher::new(
        "windows-error-reporting",
        "windows",
        windows_wer_dirs(),
        &["wer"],
    )?));
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    anyhow::bail!("this milestone supports Windows and macOS only")
}

#[cfg(target_os = "macos")]
fn macos_report_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/Library/Logs/DiagnosticReports")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Library/Logs/DiagnosticReports"));
    }
    dirs
}

#[cfg(target_os = "windows")]
fn windows_wer_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(program_data) = std::env::var_os("PROGRAMDATA") {
        let root = PathBuf::from(program_data).join("Microsoft/Windows/WER");
        dirs.push(root.join("ReportArchive"));
        dirs.push(root.join("ReportQueue"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join("Microsoft/Windows/WER/ReportArchive"));
    }
    dirs
}
