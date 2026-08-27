use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rescueloop_core::ProposedAction;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationalAction {
    RestartContainer {
        engine: String,
        container_id: String,
    },
    RestartService {
        service_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationalReceipt {
    pub id: Uuid,
    pub action: OperationalAction,
    pub previous_running: bool,
    pub verified: bool,
    pub rolled_back: bool,
}

pub fn compile_operational(proposal: &ProposedAction) -> Result<Option<OperationalAction>> {
    let string = |key: &str| {
        proposal
            .parameters
            .get(key)
            .and_then(|value| value.as_str())
            .with_context(|| format!("{} has no {key}", proposal.action_type))
    };
    let action = match proposal.action_type.as_str() {
        "restart_container" => {
            let engine = string("engine")?.to_string();
            if !matches!(engine.as_str(), "docker" | "podman") {
                bail!("unsupported container engine")
            }
            OperationalAction::RestartContainer {
                engine,
                container_id: string("container_id")?.to_string(),
            }
        }
        "restart_service" => OperationalAction::RestartService {
            service_id: string("service_id")?.to_string(),
        },
        _ => return Ok(None),
    };
    Ok(Some(action))
}

pub async fn execute_operational(
    action: OperationalAction,
    allowed_id: &str,
) -> Result<OperationalReceipt> {
    let id = match &action {
        OperationalAction::RestartContainer { container_id, .. } => container_id,
        OperationalAction::RestartService { service_id } => service_id,
    };
    if id != allowed_id || id.is_empty() {
        bail!("operational target is not the exact evidenced identity")
    }
    match &action {
        OperationalAction::RestartContainer {
            engine,
            container_id,
        } => {
            let previous_running = container_running(engine, container_id).await?;
            let status = tokio::process::Command::new(engine)
                .args(["restart", container_id])
                .status()
                .await?;
            if !status.success() {
                bail!("container engine rejected restart")
            }
            let verified = container_running(engine, container_id).await?;
            let mut rolled_back = false;
            if !verified && !previous_running {
                let _ = tokio::process::Command::new(engine)
                    .args(["stop", container_id])
                    .status()
                    .await;
                rolled_back = true;
            }
            Ok(OperationalReceipt {
                id: Uuid::new_v4(),
                action,
                previous_running,
                verified,
                rolled_back,
            })
        }
        OperationalAction::RestartService { service_id } => {
            execute_service(action.clone(), service_id).await
        }
    }
}

async fn container_running(engine: &str, id: &str) -> Result<bool> {
    let output = tokio::process::Command::new(engine)
        .args(["inspect", "--format", "{{.State.Running}}", id])
        .output()
        .await?;
    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
}

#[allow(clippy::needless_return)]
async fn execute_service(
    action: OperationalAction,
    service_id: &str,
) -> Result<OperationalReceipt> {
    #[cfg(target_os = "macos")]
    {
        let previous_running = tokio::process::Command::new("launchctl")
            .args(["print", service_id])
            .output()
            .await?
            .status
            .success();
        let status = tokio::process::Command::new("launchctl")
            .args(["kickstart", "-k", service_id])
            .status()
            .await?;
        let verified = status.success()
            && tokio::process::Command::new("launchctl")
                .args(["print", service_id])
                .output()
                .await?
                .status
                .success();
        return Ok(OperationalReceipt {
            id: Uuid::new_v4(),
            action,
            previous_running,
            verified,
            rolled_back: false,
        });
    }
    #[cfg(target_os = "windows")]
    {
        let previous_running = tokio::process::Command::new("sc.exe")
            .args(["query", service_id])
            .output()
            .await?
            .stdout
            .windows(7)
            .any(|value| value == b"RUNNING");
        let _ = tokio::process::Command::new("sc.exe")
            .args(["stop", service_id])
            .status()
            .await;
        let status = tokio::process::Command::new("sc.exe")
            .args(["start", service_id])
            .status()
            .await?;
        return Ok(OperationalReceipt {
            id: Uuid::new_v4(),
            action,
            previous_running,
            verified: status.success(),
            rolled_back: false,
        });
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    bail!("service repair supports macOS and Windows")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RepairAction {
    QuarantinePath {
        target: PathBuf,
    },
    RegenerateCache {
        target: PathBuf,
    },
    PatchJson {
        target: PathBuf,
        pointer: String,
        value: serde_json::Value,
    },
    SetPermission {
        target: PathBuf,
        mode: u32,
    },
}

impl RepairAction {
    pub fn target(&self) -> &Path {
        match self {
            Self::QuarantinePath { target }
            | Self::RegenerateCache { target }
            | Self::PatchJson { target, .. }
            | Self::SetPermission { target, .. } => target,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairPlan {
    pub action: RepairAction,
    pub reason: String,
}

pub fn compile(proposal: &ProposedAction) -> Result<RepairPlan> {
    if !proposal.reversible {
        bail!("repair proposal is not reversible")
    }
    let target = proposal
        .parameters
        .get("target")
        .and_then(|x| x.as_str())
        .context("repair proposal has no string target")?;
    let action = match proposal.action_type.as_str() {
        "quarantine_path" => RepairAction::QuarantinePath {
            target: PathBuf::from(target),
        },
        "regenerate_cache" => RepairAction::RegenerateCache {
            target: PathBuf::from(target),
        },
        "patch_json_config" => RepairAction::PatchJson {
            target: PathBuf::from(target),
            pointer: proposal
                .parameters
                .get("pointer")
                .and_then(|value| value.as_str())
                .context("JSON config repair has no pointer")?
                .to_string(),
            value: proposal
                .parameters
                .get("value")
                .context("JSON config repair has no value")?
                .clone(),
        },
        "set_permission" => RepairAction::SetPermission {
            target: PathBuf::from(target),
            mode: u32::from_str_radix(
                proposal
                    .parameters
                    .get("mode")
                    .and_then(|value| value.as_str())
                    .context("permission repair has no mode")?
                    .trim_start_matches("0o"),
                8,
            )
            .context("permission mode must be octal")?,
        },
        other => bail!("repair action is not executable in this milestone: {other}"),
    };
    if proposal.reason.trim().is_empty() {
        bail!("repair proposal has no reason")
    }
    Ok(RepairPlan {
        action,
        reason: proposal.reason.clone(),
    })
}

#[derive(Debug, Clone)]
pub struct ScopePolicy {
    allowed_roots: Vec<PathBuf>,
}

impl ScopePolicy {
    pub fn new(allowed_roots: Vec<PathBuf>) -> Result<Self> {
        if allowed_roots.is_empty() {
            bail!("at least one --allow-root is required")
        }
        let mut canonical = Vec::new();
        for root in allowed_roots {
            let root = std::fs::canonicalize(&root)
                .with_context(|| format!("invalid allowed root: {}", root.display()))?;
            if root.parent().is_none() {
                bail!("filesystem root cannot be an allowed repair scope")
            }
            canonical.push(root);
        }
        Ok(Self {
            allowed_roots: canonical,
        })
    }

    pub fn validate(&self, plan: &RepairPlan) -> Result<PathBuf> {
        let target = std::fs::canonicalize(plan.action.target()).with_context(|| {
            format!(
                "repair target does not exist: {}",
                plan.action.target().display()
            )
        })?;
        let permitted = self
            .allowed_roots
            .iter()
            .any(|root| target.starts_with(root) && target != *root);
        if !permitted {
            bail!("repair target is outside the explicitly allowed scope")
        }
        let metadata = std::fs::symlink_metadata(plan.action.target())?;
        if metadata.file_type().is_symlink() {
            bail!("symbolic-link repair targets are rejected")
        }
        Ok(target)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransactionState {
    Prepared,
    Applied,
    Verified,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub schema_version: u16,
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub state: TransactionState,
    pub action: RepairAction,
    pub original: PathBuf,
    pub backup: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_mode: Option<u32>,
}

pub async fn prepare(
    plan: &RepairPlan,
    policy: &ScopePolicy,
    transaction_root: &Path,
) -> Result<Transaction> {
    let original = policy.validate(plan)?;
    let id = Uuid::new_v4();
    let filename = original
        .file_name()
        .context("repair target has no filename")?;
    let backup = transaction_root.join(id.to_string()).join(filename);
    #[cfg(unix)]
    let original_mode = {
        use std::os::unix::fs::PermissionsExt;
        Some(std::fs::metadata(&original)?.permissions().mode() & 0o7777)
    };
    #[cfg(not(unix))]
    let original_mode = None;
    Ok(Transaction {
        schema_version: 1,
        id,
        created_at: Utc::now(),
        state: TransactionState::Prepared,
        action: plan.action.clone(),
        original,
        backup,
        original_mode,
    })
}

pub async fn apply(transaction: &mut Transaction) -> Result<()> {
    if transaction.state != TransactionState::Prepared {
        bail!("transaction is not prepared")
    }
    #[cfg(unix)]
    if let RepairAction::SetPermission { target, mode } = &transaction.action {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(target, std::fs::Permissions::from_mode(*mode)).await?;
        transaction.state = TransactionState::Applied;
        return Ok(());
    }
    #[cfg(not(unix))]
    if matches!(transaction.action, RepairAction::SetPermission { .. }) {
        bail!("POSIX permission repair is unavailable on this platform")
    }
    let parent = transaction
        .backup
        .parent()
        .context("backup has no parent")?;
    fs::create_dir_all(parent).await?;
    fs::rename(&transaction.original, &transaction.backup)
        .await
        .context(
            "backup move failed; target and transaction directory must be on the same filesystem",
        )?;
    if let RepairAction::PatchJson { pointer, value, .. } = &transaction.action {
        let result = async {
            let bytes = fs::read(&transaction.backup).await?;
            let mut document: serde_json::Value =
                serde_json::from_slice(&bytes).context("target is not valid JSON")?;
            let slot = document
                .pointer_mut(pointer)
                .with_context(|| format!("JSON pointer does not exist: {pointer}"))?;
            *slot = value.clone();
            fs::write(&transaction.original, serde_json::to_vec_pretty(&document)?).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            let _ = fs::rename(&transaction.backup, &transaction.original).await;
            return Err(error).context("failed to apply JSON config patch");
        }
    } else if matches!(transaction.action, RepairAction::RegenerateCache { .. })
        && let Err(error) = fs::create_dir(&transaction.original).await
    {
        let _ = fs::rename(&transaction.backup, &transaction.original).await;
        return Err(error).context("failed to create regenerated cache directory");
    }
    transaction.state = TransactionState::Applied;
    Ok(())
}

pub async fn rollback(transaction: &mut Transaction) -> Result<()> {
    if transaction.state != TransactionState::Applied {
        bail!("only an applied transaction can be rolled back")
    }
    #[cfg(unix)]
    if let RepairAction::SetPermission { target, .. } = &transaction.action {
        use std::os::unix::fs::PermissionsExt;
        let mode = transaction
            .original_mode
            .context("original mode was not recorded")?;
        fs::set_permissions(target, std::fs::Permissions::from_mode(mode)).await?;
        transaction.state = TransactionState::RolledBack;
        return Ok(());
    }
    #[cfg(not(unix))]
    if matches!(transaction.action, RepairAction::SetPermission { .. }) {
        bail!("POSIX permission rollback is unavailable on this platform")
    }
    if matches!(transaction.action, RepairAction::RegenerateCache { .. })
        && fs::try_exists(&transaction.original).await?
    {
        let metadata = fs::symlink_metadata(&transaction.original).await?;
        if metadata.is_dir() {
            fs::remove_dir(&transaction.original)
                .await
                .context("regenerated cache is no longer empty; refusing destructive rollback")?;
        } else {
            bail!("regenerated cache target changed type; refusing rollback")
        }
    }
    if matches!(transaction.action, RepairAction::PatchJson { .. })
        && fs::try_exists(&transaction.original).await?
    {
        fs::remove_file(&transaction.original).await?;
    }
    fs::rename(&transaction.backup, &transaction.original)
        .await
        .context("failed to restore backup")?;
    transaction.state = TransactionState::RolledBack;
    Ok(())
}

pub async fn finalize(transaction: &mut Transaction, verification_passed: bool) -> Result<()> {
    if transaction.state != TransactionState::Applied {
        bail!("transaction is not applied")
    }
    if verification_passed {
        transaction.state = TransactionState::Verified;
        Ok(())
    } else {
        rollback(transaction).await
    }
}

pub async fn persist(transaction: &Transaction, transaction_root: &Path) -> Result<PathBuf> {
    let dir = transaction_root.join(transaction.id.to_string());
    fs::create_dir_all(&dir).await?;
    let path = dir.join("transaction.json");
    fs::write(&path, serde_json::to_vec_pretty(transaction)?).await?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn plan(target: &Path, action_type: &str) -> RepairPlan {
        compile(&ProposedAction {
            action_type: action_type.into(),
            reason: "test".into(),
            parameters: json!({"target": target}),
            reversible: true,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn quarantine_and_rollback_restore_original() {
        let temp = tempdir().unwrap();
        let scope = temp.path().join("app");
        let target = scope.join("plugin");
        fs::create_dir_all(&target).await.unwrap();
        fs::write(target.join("data"), b"original").await.unwrap();
        let policy = ScopePolicy::new(vec![scope]).unwrap();
        let tx_root = temp.path().join("transactions");
        let mut tx = prepare(&plan(&target, "quarantine_path"), &policy, &tx_root)
            .await
            .unwrap();
        apply(&mut tx).await.unwrap();
        assert!(!target.exists());
        rollback(&mut tx).await.unwrap();
        assert_eq!(fs::read(target.join("data")).await.unwrap(), b"original");
    }

    #[tokio::test]
    async fn rejects_target_outside_scope() {
        let temp = tempdir().unwrap();
        let scope = temp.path().join("app");
        let target = temp.path().join("other");
        fs::create_dir_all(&scope).await.unwrap();
        fs::create_dir_all(&target).await.unwrap();
        let policy = ScopePolicy::new(vec![scope]).unwrap();
        assert!(
            prepare(
                &plan(&target, "quarantine_path"),
                &policy,
                &temp.path().join("tx")
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn failed_verification_automatically_rolls_back() {
        let temp = tempdir().unwrap();
        let scope = temp.path().join("app");
        let target = scope.join("cache");
        fs::create_dir_all(&target).await.unwrap();
        fs::write(target.join("old"), b"state").await.unwrap();
        let policy = ScopePolicy::new(vec![scope]).unwrap();
        let tx_root = temp.path().join("transactions");
        let mut tx = prepare(&plan(&target, "regenerate_cache"), &policy, &tx_root)
            .await
            .unwrap();
        apply(&mut tx).await.unwrap();
        assert!(target.exists());
        finalize(&mut tx, false).await.unwrap();
        assert_eq!(tx.state, TransactionState::RolledBack);
        assert_eq!(fs::read(target.join("old")).await.unwrap(), b"state");
    }

    #[tokio::test]
    async fn successful_verification_keeps_backup_and_marks_verified() {
        let temp = tempdir().unwrap();
        let scope = temp.path().join("app");
        let target = scope.join("plugin");
        fs::create_dir_all(&target).await.unwrap();
        let policy = ScopePolicy::new(vec![scope]).unwrap();
        let mut tx = prepare(
            &plan(&target, "quarantine_path"),
            &policy,
            &temp.path().join("transactions"),
        )
        .await
        .unwrap();
        apply(&mut tx).await.unwrap();
        finalize(&mut tx, true).await.unwrap();
        assert_eq!(tx.state, TransactionState::Verified);
        assert!(tx.backup.exists());
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn json_patch_is_typed_and_rolls_back_to_exact_bytes() {
        let temp = tempdir().unwrap();
        let scope = temp.path().join("app");
        fs::create_dir_all(&scope).await.unwrap();
        let target = scope.join("config.json");
        let original = br#"{"server":{"port":8080}}"#;
        fs::write(&target, original).await.unwrap();
        let proposal = ProposedAction {
            action_type: "patch_json_config".into(),
            reason: "fix port".into(),
            parameters: json!({"target": target, "pointer": "/server/port", "value": 8081}),
            reversible: true,
        };
        let policy = ScopePolicy::new(vec![scope]).unwrap();
        let mut tx = prepare(
            &compile(&proposal).unwrap(),
            &policy,
            &temp.path().join("transactions"),
        )
        .await
        .unwrap();
        apply(&mut tx).await.unwrap();
        let changed: serde_json::Value =
            serde_json::from_slice(&fs::read(&target).await.unwrap()).unwrap();
        assert_eq!(changed.pointer("/server/port"), Some(&json!(8081)));
        rollback(&mut tx).await.unwrap();
        assert_eq!(fs::read(&target).await.unwrap(), original);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn permission_change_restores_original_mode() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempdir().unwrap();
        let scope = temp.path().join("app");
        fs::create_dir_all(&scope).await.unwrap();
        let target = scope.join("tool");
        fs::write(&target, b"test").await.unwrap();
        fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
            .await
            .unwrap();
        let proposal = ProposedAction {
            action_type: "set_permission".into(),
            reason: "make executable".into(),
            parameters: json!({"target":target,"mode":"0755"}),
            reversible: true,
        };
        let policy = ScopePolicy::new(vec![scope]).unwrap();
        let mut tx = prepare(
            &compile(&proposal).unwrap(),
            &policy,
            &temp.path().join("tx"),
        )
        .await
        .unwrap();
        apply(&mut tx).await.unwrap();
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        rollback(&mut tx).await.unwrap();
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
