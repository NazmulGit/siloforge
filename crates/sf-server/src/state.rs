//! Shared application state handed to every handler.

use std::sync::Arc;
use std::time::Instant;

use sf_core::Config;
use sf_jobs::JobQueue;
use sf_store::Db;

use crate::auth::AuthCache;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub jobs: JobQueue,
    /// AES key for stored secrets (derived once from the master secret).
    pub secret_key: Arc<[u8; 32]>,
    pub auth_cache: AuthCache,
    pub started: Instant,
    /// Type-keyed registry for services owned by upper crates (ai, ops monitor, notifier, mcp, ...).
    pub services: Arc<Services>,
}

#[derive(Default)]
pub struct Services {
    items: parking_lot::RwLock<Vec<Arc<dyn std::any::Any + Send + Sync>>>,
}

impl Services {
    pub fn insert<T: Send + Sync + 'static>(&self, svc: Arc<T>) {
        self.items.write().push(svc);
    }
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.items
            .read()
            .iter()
            .find_map(|s| s.clone().downcast::<T>().ok())
    }
}

impl AppState {
    pub fn new(cfg: Config, db: Db, secret_key: [u8; 32]) -> Self {
        let jobs = JobQueue::new(db.clone());
        Self {
            cfg: Arc::new(cfg),
            db,
            jobs,
            secret_key: Arc::new(secret_key),
            auth_cache: AuthCache::default(),
            started: Instant::now(),
            services: Arc::new(Services::default()),
        }
    }

    /// Fetches a registered service or a 503 error.
    pub fn service<T: Send + Sync + 'static>(&self) -> Result<Arc<T>, crate::ApiError> {
        self.services.get::<T>().ok_or_else(|| {
            crate::ApiError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                format!("service {} not available", std::any::type_name::<T>()),
            )
        })
    }
}

/// MCP tool catalogue registered by the binary (sf-server does not depend on sf-mcp), listed by
/// `GET /api/v2/mcp-info` (G2-7 / A6). Each entry: `{name, title, description, read_only, destructive}`.
#[derive(Debug, Clone, Default)]
pub struct McpTools(pub Vec<serde_json::Value>);
