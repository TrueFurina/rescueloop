use anyhow::{Context, Result, bail};
use rescueloop_core::ProposedAction;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
