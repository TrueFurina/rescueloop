use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rescueloop_core::ProposedAction;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RepairAction {
    QuarantinePath { target: PathBuf },
    RegenerateCache { target: PathBuf },
}

impl RepairAction {
    pub fn target(&self) -> &Path {
        match self {
            Self::QuarantinePath { target } | Self::RegenerateCache { target } => target,
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
    Ok(Transaction {
        schema_version: 1,
        id,
        created_at: Utc::now(),
        state: TransactionState::Prepared,
        action: plan.action.clone(),
        original,
        backup,
    })
}

pub async fn apply(transaction: &mut Transaction) -> Result<()> {
    if transaction.state != TransactionState::Prepared {
        bail!("transaction is not prepared")
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
    if matches!(transaction.action, RepairAction::RegenerateCache { .. })
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
}
