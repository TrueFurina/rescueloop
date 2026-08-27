use anyhow::{Context, Result};
use rescueloop_core::Incident;
use std::path::{Path, PathBuf};
use tokio::fs;

use crate::incident_store::{ledger_path, record_incident_status};

pub(crate) async fn repair(
    incident_dir: &Path,
    incident_path: &Path,
    analysis_path: &Path,
    action_index: usize,
    allowed_roots: Vec<PathBuf>,
    approved: bool,
) -> Result<()> {
    repair_impl(
        incident_dir,
        incident_path,
        analysis_path,
        action_index,
        allowed_roots,
        approved,
        true,
    )
    .await
}

pub(crate) async fn repair_silent(
    incident_dir: &Path,
    incident_path: &Path,
    analysis_path: &Path,
    action_index: usize,
    allowed_roots: Vec<PathBuf>,
    approved: bool,
) -> Result<()> {
    repair_impl(
        incident_dir,
        incident_path,
        analysis_path,
        action_index,
        allowed_roots,
        approved,
        false,
    )
    .await
}

async fn repair_impl(
    incident_dir: &Path,
    incident_path: &Path,
    analysis_path: &Path,
    action_index: usize,
    allowed_roots: Vec<PathBuf>,
    approved: bool,
    verbose: bool,
) -> Result<()> {
    macro_rules! report {
        ($($argument:tt)*) => {
            if verbose {
                println!($($argument)*);
            }
        };
    }
    let incident: Incident = serde_json::from_slice(&fs::read(incident_path).await?)?;
    let analysis: rescueloop_core::AnalysisResponse =
        serde_json::from_slice(&fs::read(analysis_path).await?)?;
    let proposal = analysis
        .proposed_actions
        .get(action_index)
        .context("action index is out of range")?;
    if let Some(action) = rescueloop_repair::compile_operational(proposal)? {
        report!("DRY RUN: {}", serde_json::to_string_pretty(&action)?);
        if !approved {
            report!("No changes made. Approve this exact operational target to execute.");
            return Ok(());
        }
        let target_id = match &action {
            rescueloop_repair::OperationalAction::RestartContainer { container_id, .. } => {
                container_id.clone()
            }
            rescueloop_repair::OperationalAction::RestartService { service_id } => {
                service_id.clone()
            }
        };
        let evidenced = incident.evidence.iter().any(|evidence| {
            evidence
                .fields
                .values()
                .any(|value| value.as_str() == Some(target_id.as_str()))
        });
        if !evidenced {
            anyhow::bail!("operational target is not present in incident evidence")
        }
        record_incident_status(
            incident_dir,
            &incident,
            rescueloop_core::IncidentStatus::RepairApplied,
            None,
        )
        .await?;
        let receipt = rescueloop_repair::execute_operational(action, &target_id).await?;
        record_incident_status(
            incident_dir,
            &incident,
            rescueloop_core::IncidentStatus::VerificationPending,
            None,
        )
        .await?;
        let transaction_root = incident_dir
            .parent()
            .unwrap_or(incident_dir)
            .join("transactions")
            .join(receipt.id.to_string());
        fs::create_dir_all(&transaction_root).await?;
        let receipt_path = transaction_root.join("operational-receipt.json");
        fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt)?).await?;
        let status = if receipt.verified {
            rescueloop_core::IncidentStatus::VerifiedFixed
        } else {
            rescueloop_core::IncidentStatus::VerificationFailed
        };
        record_incident_status(
            incident_dir,
            &incident,
            status,
            Some(serde_json::to_value(&receipt)?),
        )
        .await?;
        if !receipt.verified {
            anyhow::bail!("operational repair failed verification")
        }
        report!(
            "VERIFIED operational repair. Receipt: {}",
            receipt_path.display()
        );
        return Ok(());
    }
    let plan = rescueloop_repair::compile(proposal)?;
    let proposed_target = std::fs::canonicalize(plan.action.target()).with_context(|| {
        format!(
            "repair target does not exist: {}",
            plan.action.target().display()
        )
    })?;
    let target_is_evidenced = incident.evidence.iter().any(|evidence| {
        evidence
            .artifact
            .as_ref()
            .and_then(|path| std::fs::canonicalize(path).ok())
            .is_some_and(|path| path == proposed_target)
    });
    if !target_is_evidenced {
        anyhow::bail!(
            "filesystem repair target is not the exact artifact recorded in incident evidence"
        )
    }
    let policy = rescueloop_repair::ScopePolicy::new(allowed_roots)?;
    let transaction_root = incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join("transactions");
    let mut transaction = rescueloop_repair::prepare(&plan, &policy, &transaction_root).await?;
    report!("DRY RUN: {}", serde_json::to_string_pretty(&transaction)?);
    if !approved {
        report!("No changes made. Review the exact target and repeat with --approve to execute.");
        return Ok(());
    }
    let launch_context = incident
        .launch_context
        .clone()
        .context("verified repair requires an exact recorded launch context")?;
    rescueloop_repair::apply(&mut transaction).await?;
    record_incident_status(
        incident_dir,
        &incident,
        rescueloop_core::IncidentStatus::RepairApplied,
        None,
    )
    .await?;
    rescueloop_repair::persist(&transaction, &transaction_root).await?;
    report!(
        "APPLIED: backup created at {}",
        transaction.backup.display()
    );

    record_incident_status(
        incident_dir,
        &incident,
        rescueloop_core::IncidentStatus::VerificationPending,
        None,
    )
    .await?;
    let replay = rescueloop_platform::verify_replay(&launch_context).await;
    match replay {
        Ok(result) if result.passed => {
            rescueloop_repair::finalize(&mut transaction, true).await?;
            let receipt = rescueloop_repair::persist(&transaction, &transaction_root).await?;
            report!(
                "VERIFIED: original action now succeeds ({} ms).",
                result.duration_ms
            );
            report!("Transaction receipt: {}", receipt.display());
            record_repair_lineage(
                incident_dir,
                &incident,
                &transaction,
                rescueloop_core::IncidentStatus::VerifiedFixed,
                serde_json::json!({"passed": true, "exit_code": result.exit_code, "duration_ms": result.duration_ms}),
                verbose,
            )
            .await?;
        }
        result => {
            let replay_message = match result {
                Ok(value) => format!("exit code {:?}", value.exit_code),
                Err(error) => error.to_string(),
            };
            rescueloop_repair::finalize(&mut transaction, false)
                .await
                .with_context(|| {
                    format!(
                        "CRITICAL: verification failed ({replay_message}) and automatic rollback also failed"
                    )
                })?;
            let receipt = rescueloop_repair::persist(&transaction, &transaction_root).await?;
            report!(
                "ROLLED BACK: verification failed ({replay_message}); original state restored."
            );
            report!("Transaction receipt: {}", receipt.display());
            record_repair_lineage(
                incident_dir,
                &incident,
                &transaction,
                rescueloop_core::IncidentStatus::RolledBack,
                serde_json::json!({"passed": false, "detail": replay_message}),
                verbose,
            )
            .await?;
        }
    }
    Ok(())
}

async fn record_repair_lineage(
    incident_dir: &Path,
    incident: &Incident,
    transaction: &rescueloop_repair::Transaction,
    status: rescueloop_core::IncidentStatus,
    verifier: serde_json::Value,
    verbose: bool,
) -> Result<()> {
    let entry = rescueloop_ledger::append(
        &ledger_path(incident_dir),
        rescueloop_ledger::NewLedgerEntry {
            incident: incident.clone(),
            repair: Some(serde_json::to_value(&transaction.action)?),
            before_state: Some(serde_json::json!({"original": transaction.original})),
            after_state: Some(serde_json::json!({"backup": transaction.backup, "transaction_state": transaction.state})),
            verifier: Some(verifier),
            status,
            relation_override: None,
        },
    )
    .await?;
    if verbose {
        println!("LINEAGE: {:?}", entry.relation);
    }
    Ok(())
}
