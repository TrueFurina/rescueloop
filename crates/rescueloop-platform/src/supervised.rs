use anyhow::{Context, Result, bail};
use rescueloop_core::{Confidence, Evidence, Incident, IncidentKind, LaunchContext};
use std::process::Stdio;
use std::{collections::BTreeMap, path::Path, time::Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

pub async fn supervise(
    executable: &Path,
    args: &[String],
    record_args: bool,
) -> Result<Option<Incident>> {
    supervise_inner(executable, args, record_args, true).await
}

pub async fn supervise_quiet(
    executable: &Path,
    args: &[String],
    record_args: bool,
) -> Result<Option<Incident>> {
    supervise_inner(executable, args, record_args, false).await
}

async fn supervise_inner(
    executable: &Path,
    args: &[String],
    record_args: bool,
    echo_output: bool,
) -> Result<Option<Incident>> {
    let started = Instant::now();
    let mut child = Command::new(executable)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to launch {}", executable.display()))?;
    let stdout = child
        .stdout
        .take()
        .context("failed to capture process stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture process stderr")?;
    let (status, stdout, stderr) =
        tokio::join!(child.wait(), drain_bounded(stdout), drain_bounded(stderr));
    let status = status?;
    let stdout = stdout?;
    let stderr = stderr?;
    if echo_output && !stdout.is_empty() {
        print!("{}", String::from_utf8_lossy(&stdout));
    }
    if echo_output && !stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&stderr));
    }
    if status.success() {
        return Ok(None);
    }

    let mut fields = BTreeMap::new();
    fields.insert("exit_code".into(), serde_json::json!(status.code()));
    fields.insert(
        "duration_ms".into(),
        serde_json::json!(started.elapsed().as_millis()),
    );
    fields.insert(
        "diagnostic_output".into(),
        serde_json::json!(diagnostic_output_lines(&stdout, &stderr)),
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        fields.insert("signal".into(), serde_json::json!(status.signal()));
    }
    let application = executable
        .file_name()
        .and_then(|x| x.to_str())
        .map(str::to_owned);
    let mut incident = Incident::detected(
        std::env::consts::OS,
        IncidentKind::AbnormalExit,
        format!(
            "Detected non-successful exit from {}",
            application.as_deref().unwrap_or("process")
        ),
        Evidence {
            source: "supervised-process".into(),
            summary: "The launched process returned a non-success status".into(),
            artifact: None,
            fields,
        },
    );
    incident.application = application;
    incident.confidence = Confidence::Confirmed;
    incident.normalized_failure.code = status.code().map(|code| format!("exit:{code}"));
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            incident.normalized_failure.code = Some(format!("signal:{signal}"));
        }
    }
    incident.launch_context = Some(LaunchContext {
        executable: executable.to_path_buf(),
        arguments: record_args.then(|| args.to_vec()),
        working_directory: std::env::current_dir().ok(),
    });
    Ok(Some(incident))
}

async fn drain_bounded(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    const RETAIN_LIMIT: usize = 16 * 1024;
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = RETAIN_LIMIT.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(retained)
}

fn diagnostic_output_lines(stdout: &[u8], stderr: &[u8]) -> Vec<String> {
    const KEYS: &[&str] = &[
        "error",
        "exception",
        "fail",
        "plugin",
        "module",
        "missing",
        "permission",
        "runtime",
    ];
    crate::diagnostics::select_lines(
        &String::from_utf8_lossy(&[stdout, stderr].concat()),
        KEYS,
        &[],
        20,
    )
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ReplayResult {
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
}

pub async fn verify_replay(context: &LaunchContext) -> Result<ReplayResult> {
    let Some(args) = &context.arguments else {
        bail!("arguments were not recorded; exact replay is unavailable")
    };
    let started = Instant::now();
    let mut command = Command::new(&context.executable);
    command
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = &context.working_directory {
        command.current_dir(dir);
    }
    let status = command
        .status()
        .await
        .context("failed to replay original action")?;
    Ok(ReplayResult {
        passed: status.success(),
        exit_code: status.code(),
        duration_ms: started.elapsed().as_millis(),
    })
}

#[cfg(test)]
mod tests {
    use super::diagnostic_output_lines;

    #[test]
    fn keeps_error_evidence_and_drops_unrelated_output() {
        let lines = diagnostic_output_lines(
            b"normal progress\n",
            b"Plugin error: broken.plugin\nuser text\n",
        );
        assert_eq!(lines, vec!["Plugin error: broken.plugin"]);
    }
}
