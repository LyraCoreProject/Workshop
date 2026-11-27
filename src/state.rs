use crate::config::Config;
use crate::snapshot::Snapshot;
use chrono::{DateTime, Utc};
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, Default)]
pub struct Health {
    pub last_error: Option<(DateTime<Utc>, String)>,
    pub last_request_count: usize,
}

/// The current snapshot and how the last refresh went. Nothing else is kept.
pub struct AppState {
    pub config: Config,
    snapshot: RwLock<Option<Arc<Snapshot>>>,
    health: RwLock<Health>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            snapshot: RwLock::new(None),
            health: RwLock::new(Health::default()),
        }
    }

    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.snapshot.read().unwrap().clone()
    }

    pub fn health(&self) -> Health {
        self.health.read().unwrap().clone()
    }

    pub fn replace(&self, snapshot: Snapshot, request_count: usize) {
        *self.snapshot.write().unwrap() = Some(Arc::new(snapshot));
        *self.health.write().unwrap() = Health {
            last_error: None,
            last_request_count: request_count,
        };
    }

    /// A failed refresh keeps the previous snapshot.
    pub fn record_error(&self, at: DateTime<Utc>, error: String, request_count: usize) {
        *self.health.write().unwrap() = Health {
            last_error: Some((at, error)),
            last_request_count: request_count,
        };
    }
}
