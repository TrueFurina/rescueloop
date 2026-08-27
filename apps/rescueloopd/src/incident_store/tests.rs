use super::*;
use rescueloop_core::{Evidence, IncidentKind};
use std::collections::BTreeMap;

fn fixture(application: &str, code: &str) -> Incident {
    let mut incident = Incident::detected(
        "test",
        IncidentKind::Crash,
        "failure",
        Evidence {
            source: "test".into(),
            summary: "failure".into(),
            artifact: None,
            fields: BTreeMap::new(),
        },
    );
    incident.application = Some(application.into());
    incident.normalized_failure.code = Some(code.into());
    incident
}

#[tokio::test]
async fn indexed_grouping_ignores_unrelated_broken_projection() {
    let root = std::env::temp_dir().join(format!("rescueloop-store-{}", uuid::Uuid::new_v4()));
    let directory = root.join("incidents");
    let first = fixture("api", "oom");
    let (first_path, created) = save_incident(&directory, &first).await.unwrap();
    assert!(created);
    let unrelated = fixture("worker", "panic");
    let (unrelated_path, created) = save_incident(&directory, &unrelated).await.unwrap();
    assert!(created);
    fs::write(&unrelated_path, b"broken unrelated JSON")
        .await
        .unwrap();
    let recurrence = fixture("api", "oom");
    let (grouped_path, created) = save_incident(&directory, &recurrence).await.unwrap();
    assert!(!created);
    assert_eq!(grouped_path, first_path);
    let grouped: Incident = serde_json::from_slice(&fs::read(first_path).await.unwrap()).unwrap();
    assert_eq!(grouped.occurrence_count, 2);
    fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_grouping_preserves_every_occurrence() {
    let root = std::env::temp_dir().join(format!("rescueloop-store-{}", uuid::Uuid::new_v4()));
    let directory = root.join("incidents");
    let tasks = (0..16)
        .map(|_| {
            let directory = directory.clone();
            tokio::spawn(async move { save_incident(&directory, &fixture("api", "oom")).await })
        })
        .collect::<Vec<_>>();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    let stored = incidents(&directory).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].0.occurrence_count, 16);
    let mut occurrences = fs::read_dir(root.join("occurrences")).await.unwrap();
    let mut occurrence_count = 0;
    while occurrences.next_entry().await.unwrap().is_some() {
        occurrence_count += 1;
    }
    assert_eq!(occurrence_count, 16);
    fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn duplicate_occurrence_is_idempotent() {
    let root = std::env::temp_dir().join(format!("rescueloop-store-{}", uuid::Uuid::new_v4()));
    let directory = root.join("incidents");
    let occurrence = fixture("api", "oom");
    save_incident(&directory, &occurrence).await.unwrap();
    let (_, created) = save_incident(&directory, &occurrence).await.unwrap();
    assert!(!created);
    let stored = incidents(&directory).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].0.occurrence_count, 1);
    fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn recovers_journal_before_occurrence_publication() {
    let root = std::env::temp_dir().join(format!("rescueloop-store-{}", uuid::Uuid::new_v4()));
    let directory = root.join("incidents");
    let occurrence = fixture("api", "oom");
    observation_journal::begin(&directory, &occurrence)
        .await
        .unwrap();
    assert_eq!(recover_pending_observations(&directory).await.unwrap(), 1);
    assert_eq!(recover_pending_observations(&directory).await.unwrap(), 0);
    let stored = incidents(&directory).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].0.last_occurrence_id, Some(occurrence.id));
    assert!(occurrence_path(&directory, occurrence.id).exists());
    assert_eq!(
        rescueloop_ledger::load(&ledger_path(&directory))
            .await
            .unwrap()
            .len(),
        1
    );
    fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn recovery_does_not_reapply_a_persisted_projection() {
    let root = std::env::temp_dir().join(format!("rescueloop-store-{}", uuid::Uuid::new_v4()));
    let directory = root.join("incidents");
    let occurrence = fixture("api", "oom");
    observation_journal::begin(&directory, &occurrence)
        .await
        .unwrap();
    {
        let _lock = acquire_store_lock(&directory).await.unwrap();
        apply_observation(&directory, &occurrence).await.unwrap();
    }
    assert_eq!(recover_pending_observations(&directory).await.unwrap(), 1);
    let stored = incidents(&directory).await.unwrap();
    assert_eq!(stored[0].0.occurrence_count, 1);
    assert_eq!(
        rescueloop_ledger::load(&ledger_path(&directory))
            .await
            .unwrap()
            .len(),
        1
    );
    fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn rejects_oversized_incident_document_without_allocating_it() {
    let root = std::env::temp_dir().join(format!("rescueloop-store-{}", uuid::Uuid::new_v4()));
    let path = root.join("oversized.json");
    fs::create_dir_all(&root).await.unwrap();
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_INCIDENT_DOCUMENT_BYTES + 1).unwrap();
    assert!(
        read_bounded_document(&path, MAX_INCIDENT_DOCUMENT_BYTES)
            .await
            .is_err()
    );
    fs::remove_dir_all(root).await.unwrap();
}
