mod operational;
mod plan;
mod transaction;

pub use operational::{
    OperationalAction, OperationalReceipt, compile_operational, execute_operational,
};
pub use plan::{RepairAction, RepairPlan, ScopePolicy, compile};
pub use transaction::{Transaction, TransactionState, apply, finalize, persist, prepare, rollback};

#[cfg(test)]
mod tests {
    use super::*;
    use rescueloop_core::ProposedAction;
    use serde_json::json;
    use std::path::Path;
    use tempfile::tempdir;
    use tokio::fs;

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
