use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;
use uuid::Uuid;

use crate::{RepairAction, RepairPlan, ScopePolicy};

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
