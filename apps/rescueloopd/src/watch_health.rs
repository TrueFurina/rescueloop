use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Default)]
pub struct WatchHealth {
    active_sources: AtomicUsize,
    degraded_sources: AtomicUsize,
    retry_count: AtomicU64,
    received: AtomicU64,
    persisted: AtomicU64,
    grouped: AtomicU64,
    queue_depth: AtomicUsize,
}

pub struct Snapshot {
    pub active_sources: usize,
    pub degraded_sources: usize,
    pub retry_count: u64,
    pub received: u64,
    pub persisted: u64,
    pub grouped: u64,
    pub queue_depth: usize,
}

impl WatchHealth {
    pub fn source_started(&self) {
        self.active_sources.fetch_add(1, Ordering::Relaxed);
    }

    pub fn source_degraded(&self) {
        self.degraded_sources.fetch_add(1, Ordering::Relaxed);
    }

    pub fn source_recovered(&self) {
        saturating_decrement(&self.degraded_sources);
    }

    pub fn source_stopped(&self, degraded: bool) {
        saturating_decrement(&self.active_sources);
        if degraded {
            saturating_decrement(&self.degraded_sources);
        }
    }

    pub fn retrying(&self) {
        self.retry_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn observation_received(&self) {
        self.received.fetch_add(1, Ordering::Relaxed);
    }

    pub fn persisted(&self) {
        self.persisted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn grouped(&self) {
        self.grouped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn queued(&self) {
        self.queue_depth.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dequeued(&self) {
        saturating_decrement(&self.queue_depth);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            active_sources: self.active_sources.load(Ordering::Relaxed),
            degraded_sources: self.degraded_sources.load(Ordering::Relaxed),
            retry_count: self.retry_count.load(Ordering::Relaxed),
            received: self.received.load(Ordering::Relaxed),
            persisted: self.persisted.load(Ordering::Relaxed),
            grouped: self.grouped.load(Ordering::Relaxed),
            queue_depth: self.queue_depth.load(Ordering::Relaxed),
        }
    }
}

fn saturating_decrement(value: &AtomicUsize) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        current.checked_sub(1)
    });
}

#[cfg(test)]
mod tests {
    use super::WatchHealth;

    #[test]
    fn tracks_source_and_queue_health() {
        let health = WatchHealth::default();
        health.source_started();
        health.source_degraded();
        health.retrying();
        health.observation_received();
        health.queued();
        health.dequeued();
        health.source_recovered();
        health.persisted();
        let snapshot = health.snapshot();
        assert_eq!(snapshot.active_sources, 1);
        assert_eq!(snapshot.degraded_sources, 0);
        assert_eq!(snapshot.retry_count, 1);
        assert_eq!(snapshot.received, 1);
        assert_eq!(snapshot.persisted, 1);
        assert_eq!(snapshot.queue_depth, 0);
    }
}
