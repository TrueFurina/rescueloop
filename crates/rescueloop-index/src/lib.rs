use anyhow::{Context, Result, bail};
use rescueloop_core::Incident;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::path::{Path, PathBuf};

const INDEX_SCHEMA: u32 = 1;
const INDEX_FILENAME: &str = "index-v1.db";

#[derive(Clone, Debug)]
pub struct IncidentIndex {
    path: PathBuf,
    incident_dir: PathBuf,
}

impl IncidentIndex {
    #[tracing::instrument(name = "index.open", skip_all, err)]
    pub async fn open(state_root: &Path, incident_dir: &Path) -> Result<Self> {
        let index = Self {
            path: state_root.join(INDEX_FILENAME),
            incident_dir: incident_dir.to_path_buf(),
        };
        let path = index.path.clone();
        let incident_dir = index.incident_dir.clone();
        tokio::task::spawn_blocking(move || open_or_rebuild(&path, &incident_dir)).await??;
        Ok(index)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[tracing::instrument(name = "index.upsert", skip(self, incident), fields(incident_id = %incident.id), err)]
    pub async fn upsert(&self, incident: &Incident, json_path: &Path) -> Result<()> {
        let path = self.path.clone();
        let incident_dir = self.incident_dir.clone();
        let incident = incident.clone();
        let json_path = json_path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_connection(&path)?;
            let transaction = connection.transaction()?;
            upsert(&transaction, &incident, &json_path)?;
            set_directory_stamp(&transaction, &incident_dir)?;
            transaction.commit()?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    pub async fn paths_newest_first(&self) -> Result<Vec<PathBuf>> {
        let path = self.path.clone();
        let incident_dir = self.incident_dir.clone();
        tokio::task::spawn_blocking(move || {
            open_or_rebuild(&path, &incident_dir)?;
            let connection = open_connection(&path)?;
            let mut query = connection.prepare(
                "SELECT json_path FROM incidents ORDER BY last_observed_at DESC, observed_at DESC",
            )?;
            let rows = query.query_map([], |row| row.get::<_, String>(0))?;
            Ok::<_, anyhow::Error>(rows.flatten().map(PathBuf::from).collect())
        })
        .await?
    }

    #[tracing::instrument(name = "index.rebuild", skip(self), err)]
    pub async fn rebuild(&self) -> Result<usize> {
        let path = self.path.clone();
        let incident_dir = self.incident_dir.clone();
        tokio::task::spawn_blocking(move || rebuild(&path, &incident_dir)).await?
    }

    pub async fn count(&self) -> Result<u64> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_connection(&path)?;
            Ok::<_, anyhow::Error>(connection.query_row(
                "SELECT COUNT(*) FROM incidents",
                [],
                |row| row.get(0),
            )?)
        })
        .await?
    }
}

fn open_or_rebuild(path: &Path, incident_dir: &Path) -> Result<()> {
    if !path.exists() {
        rebuild(path, incident_dir)?;
        return Ok(());
    }
    let healthy = open_connection(path)
        .and_then(|connection| {
            let integrity: String =
                connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
            let version: u32 =
                connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
            Ok(integrity == "ok" && version == INDEX_SCHEMA)
        })
        .unwrap_or(false);
    if !healthy {
        quarantine_broken_index(path)?;
        rebuild(path, incident_dir)?;
        return Ok(());
    }
    let connection = open_connection(path)?;
    let indexed_stamp: Option<String> = connection
        .query_row(
            "SELECT value FROM metadata WHERE key='incident_directory_stamp'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if indexed_stamp.as_deref() != directory_stamp(incident_dir)?.as_deref() {
        drop(connection);
        rebuild(path, incident_dir)?;
    }
    Ok(())
}

fn rebuild(path: &Path, incident_dir: &Path) -> Result<usize> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("rebuild-{}.db", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut connection = open_connection(&temporary)?;
        initialize_schema(&connection)?;
        let transaction = connection.transaction()?;
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(incident_dir) {
            for entry in entries.flatten() {
                let json_path = entry.path();
                if json_path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&json_path) else {
                    continue;
                };
                let Ok(incident) = serde_json::from_slice::<Incident>(&bytes) else {
                    continue;
                };
                upsert(&transaction, &incident, &json_path)?;
                count += 1;
            }
        }
        set_directory_stamp(&transaction, incident_dir)?;
        transaction.commit()?;
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(connection);
        replace_index(&temporary, path)?;
        Ok(count)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
        let _ = std::fs::remove_file(temporary.with_extension("db-wal"));
        let _ = std::fs::remove_file(temporary.with_extension("db-shm"));
    }
    result
}

fn initialize_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE incidents (
           id TEXT PRIMARY KEY,
           json_path TEXT NOT NULL UNIQUE,
           observed_at TEXT NOT NULL,
           last_observed_at TEXT NOT NULL,
           group_key TEXT NOT NULL,
           application TEXT,
           kind TEXT NOT NULL,
           status TEXT NOT NULL,
           occurrence_count INTEGER NOT NULL
         );
         CREATE INDEX incidents_group_time ON incidents(group_key, last_observed_at);
         PRAGMA user_version=1;",
    )?;
    Ok(())
}

fn open_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path)
        .with_context(|| format!("cannot open disposable incident index: {}", path.display()))?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(connection)
}

fn upsert(transaction: &Transaction<'_>, incident: &Incident, json_path: &Path) -> Result<()> {
    transaction.execute(
        "INSERT INTO incidents
         (id, json_path, observed_at, last_observed_at, group_key, application, kind, status, occurrence_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
           json_path=excluded.json_path,
           last_observed_at=excluded.last_observed_at,
           group_key=excluded.group_key,
           application=excluded.application,
           kind=excluded.kind,
           status=excluded.status,
           occurrence_count=excluded.occurrence_count",
        params![
            incident.id.to_string(),
            json_path.to_string_lossy(),
            incident.observed_at.to_rfc3339(),
            incident.last_observed_at.unwrap_or(incident.observed_at).to_rfc3339(),
            incident.group_key,
            incident.application,
            format!("{:?}", incident.kind),
            format!("{:?}", incident.status),
            incident.occurrence_count,
        ],
    )?;
    Ok(())
}

fn set_directory_stamp(transaction: &Transaction<'_>, incident_dir: &Path) -> Result<()> {
    let stamp = directory_stamp(incident_dir)?.unwrap_or_default();
    transaction.execute(
        "INSERT INTO metadata(key, value) VALUES ('incident_directory_stamp', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [stamp],
    )?;
    Ok(())
}

fn directory_stamp(path: &Path) -> Result<Option<String>> {
    let Ok(metadata) = std::fs::metadata(path) else {
        return Ok(None);
    };
    let modified = metadata.modified()?.duration_since(std::time::UNIX_EPOCH)?;
    Ok(Some(format!(
        "{}:{}",
        modified.as_secs(),
        modified.subsec_nanos()
    )))
}

fn quarantine_broken_index(path: &Path) -> Result<()> {
    move_index_family(path, "corrupt")
}

fn move_index_family(path: &Path, label: &str) -> Result<()> {
    let generation = uuid::Uuid::new_v4();
    let destination = path.with_extension(format!("{label}-{generation}.db"));
    std::fs::rename(path, destination)?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
        if sidecar.exists() {
            let destination =
                PathBuf::from(format!("{}.{}-{}", sidecar.display(), label, generation));
            let _ = std::fs::rename(sidecar, destination);
        }
    }
    Ok(())
}

fn replace_index(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        move_index_family(destination, "replaced")?;
    }
    std::fs::rename(source, destination)?;
    if !destination.exists() {
        bail!("rebuilt index was not installed")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rescueloop_core::{Evidence, IncidentKind};
    use std::collections::BTreeMap;

    fn incident(name: &str) -> Incident {
        let mut incident = Incident::detected(
            "test",
            IncidentKind::Crash,
            "boom",
            Evidence {
                source: "test".into(),
                summary: "boom".into(),
                artifact: None,
                fields: BTreeMap::new(),
            },
        );
        incident.application = Some(name.into());
        incident.group_key = name.into();
        incident
    }

    fn write_incident(directory: &Path, incident: &Incident) -> PathBuf {
        std::fs::create_dir_all(directory).unwrap();
        let path = directory.join(format!("{}.json", incident.id));
        std::fs::write(&path, serde_json::to_vec_pretty(incident).unwrap()).unwrap();
        path
    }

    #[tokio::test]
    async fn builds_projection_from_json_source_of_truth() {
        let temp = tempfile::tempdir().unwrap();
        let incidents = temp.path().join("incidents");
        let first = incident("first");
        let path = write_incident(&incidents, &first);
        let index = IncidentIndex::open(temp.path(), &incidents).await.unwrap();
        assert_eq!(index.paths_newest_first().await.unwrap(), vec![path]);
        assert!(index.path().ends_with("index-v1.db"));
    }

    #[tokio::test]
    async fn reconciles_a_json_created_after_the_index() {
        let temp = tempfile::tempdir().unwrap();
        let incidents = temp.path().join("incidents");
        std::fs::create_dir_all(&incidents).unwrap();
        let index = IncidentIndex::open(temp.path(), &incidents).await.unwrap();
        assert!(index.paths_newest_first().await.unwrap().is_empty());
        write_incident(&incidents, &incident("later"));
        assert_eq!(index.paths_newest_first().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn quarantines_corruption_and_rebuilds_without_touching_json() {
        let temp = tempfile::tempdir().unwrap();
        let incidents = temp.path().join("incidents");
        let source = incident("safe");
        let source_path = write_incident(&incidents, &source);
        let index = IncidentIndex::open(temp.path(), &incidents).await.unwrap();
        std::fs::write(index.path(), b"not sqlite").unwrap();
        let reopened = IncidentIndex::open(temp.path(), &incidents).await.unwrap();
        assert_eq!(
            reopened.paths_newest_first().await.unwrap(),
            vec![source_path.clone()]
        );
        assert!(source_path.exists());
        assert!(
            std::fs::read_dir(temp.path())
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().contains("corrupt-"))
        );
    }

    #[tokio::test]
    async fn versioned_index_does_not_touch_a_future_schema_file() {
        let temp = tempfile::tempdir().unwrap();
        let incidents = temp.path().join("incidents");
        std::fs::create_dir_all(&incidents).unwrap();
        let future = temp.path().join("index-v2.db");
        std::fs::write(&future, b"future schema sentinel").unwrap();
        IncidentIndex::open(temp.path(), &incidents).await.unwrap();
        assert_eq!(std::fs::read(future).unwrap(), b"future schema sentinel");
    }
}
