use anyhow::Result;
use rescueloop_core::{Incident, IncidentCollector};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{sync::mpsc, task::JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{console::load_settings, incident_store::save_incident, watch_health::WatchHealth};

const EVENT_QUEUE_CAPACITY: usize = 256;
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run(directory: &Path) -> Result<()> {
    tokio::fs::create_dir_all(directory).await?;
    let settings = load_settings(directory).await?;
    let sources = rescueloop_platform::event_sources(&settings.enabled_sources)?;
    let source_names = sources
        .iter()
        .map(|source| source.name())
        .collect::<Vec<_>>();
    announce(directory, &source_names);

    let (sender, mut events) = mpsc::channel(EVENT_QUEUE_CAPACITY);
    let health = Arc::new(WatchHealth::default());
    let cancellation = CancellationToken::new();
    let mut tasks = JoinSet::new();
    spawn_heartbeat(
        &mut tasks,
        source_names.len(),
        Arc::clone(&health),
        cancellation.clone(),
    );
    for source in sources {
        tasks.spawn(run_source(
            source,
            sender.clone(),
            Arc::clone(&health),
            cancellation.clone(),
        ));
    }
    drop(sender);

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let exhausted = loop {
        tokio::select! {
            signal = &mut shutdown => {
                signal?;
                info!(event = "watch.shutdown_requested", "Watcher shutdown requested");
                break false;
            }
            event = events.recv() => match event {
                Some(incident) => persist(directory, incident, &health).await?,
                None => break true,
            }
        }
    };

    cancellation.cancel();
    let drain = async {
        while let Some(incident) = events.recv().await {
            persist(directory, incident, &health).await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    match tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, drain).await {
        Ok(result) => result?,
        Err(_) => warn!(
            event = "watch.drain_timeout",
            queue_depth = health.snapshot().queue_depth,
            "Watcher shutdown drain timed out"
        ),
    }
    while tasks.join_next().await.is_some() {}

    if exhausted {
        error!(
            event = "watch.sources_exhausted",
            "All event sources stopped"
        );
        anyhow::bail!("all event sources stopped")
    }
    info!(event = "watch.stopped", "Watcher stopped cleanly");
    Ok(())
}

fn announce(directory: &Path, sources: &[&str]) {
    info!(event = "watch.ready", sources = ?sources, "Watcher initialized");
    println!("RescueLoop {}", env!("CARGO_PKG_VERSION"));
    println!("Status: READY — monitoring for objective failures");
    println!(
        "Platform: {} ({})",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("Event sources: {}", sources.join(", "));
    println!("Incidents: {}", directory.display());
    println!("Privacy: local detection only; AI analysis starts only on request");
    println!("Waiting for a new failure event...\n");
}

fn spawn_heartbeat(
    tasks: &mut JoinSet<()>,
    source_count: usize,
    health: Arc<WatchHealth>,
    cancellation: CancellationToken,
) {
    tasks.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.tick().await;
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = interval.tick() => {
                    let snapshot = health.snapshot();
                    info!(
                        event = "watch.heartbeat",
                        source_count,
                        active_sources = snapshot.active_sources,
                        degraded_sources = snapshot.degraded_sources,
                        retry_count = snapshot.retry_count,
                        received = snapshot.received,
                        persisted = snapshot.persisted,
                        grouped = snapshot.grouped,
                        queue_depth = snapshot.queue_depth,
                        "Watcher is alive"
                    );
                }
            }
        }
    });
}

async fn run_source(
    mut source: Box<dyn IncidentCollector>,
    sender: mpsc::Sender<Incident>,
    health: Arc<WatchHealth>,
    cancellation: CancellationToken,
) {
    health.source_started();
    info!(
        event = "source.started",
        source = source.name(),
        "Event source started"
    );
    let mut retry_delay = Duration::from_secs(2);
    let mut degraded = false;
    loop {
        let result = tokio::select! {
            _ = cancellation.cancelled() => break,
            result = source.next_incident() => result,
        };
        match result {
            Ok(incident) => {
                if degraded {
                    info!(
                        event = "source.recovered",
                        source = source.name(),
                        "Event source recovered"
                    );
                    degraded = false;
                    health.source_recovered();
                }
                retry_delay = Duration::from_secs(2);
                info!(event = "observation.received", source = source.name(), incident_id = %incident.id, kind = ?incident.kind, "Failure observation received");
                health.observation_received();
                let sent = tokio::select! {
                    _ = cancellation.cancelled() => false,
                    result = sender.send(incident) => result.is_ok(),
                };
                if !sent {
                    break;
                }
                health.queued();
            }
            Err(error) => {
                if !degraded {
                    health.source_degraded();
                }
                degraded = true;
                health.retrying();
                warn!(event = "source.retrying", source = source.name(), error = %format!("{error:#}"), retry_delay_ms = retry_delay.as_millis(), "Event source failed; reconnecting");
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = tokio::time::sleep(retry_delay) => {}
                }
                retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
            }
        }
    }
    health.source_stopped(degraded);
    info!(
        event = "source.stopped",
        source = source.name(),
        reason = "shutdown",
        "Event source stopped"
    );
}

async fn persist(directory: &Path, incident: Incident, health: &WatchHealth) -> Result<()> {
    health.dequeued();
    let (destination, created) = save_incident(directory, &incident).await?;
    if !created {
        health.grouped();
        info!(event = "incident.grouped", incident_id = %incident.id, "Incident grouped with an active failure");
        return Ok(());
    }
    health.persisted();
    info!(event = "incident.persisted", incident_id = %incident.id, kind = ?incident.kind, "New incident persisted");
    println!("DETECTED: {:?}: {}", incident.kind, incident.message);
    println!("Incident saved to {}", destination.display());
    println!(
        "Analysis has NOT started. Run: rescueloop analyze '{}' --endpoint <URL>",
        destination.display()
    );
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct PendingSource;

    #[async_trait]
    impl IncidentCollector for PendingSource {
        fn name(&self) -> &str {
            "pending-test"
        }

        async fn next_incident(&mut self) -> Result<Incident> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn cancellation_stops_idle_source_without_leaking_task() {
        let (sender, _events) = mpsc::channel(1);
        let health = Arc::new(WatchHealth::default());
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(run_source(
            Box::new(PendingSource),
            sender,
            Arc::clone(&health),
            cancellation.clone(),
        ));
        tokio::task::yield_now().await;
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("source task did not stop")
            .unwrap();
        assert_eq!(health.snapshot().active_sources, 0);
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}
