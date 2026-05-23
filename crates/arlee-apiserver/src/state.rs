//! In-memory Apiserver state: Edge registry + sandbox→edge mapping.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use arlee_models::EdgeInfo;
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct EdgeRecord {
    pub edge_id: String,
    pub url: String,
    pub sandbox_count: u32,
    pub last_seen: DateTime<Utc>,
    pub sandboxes: HashSet<String>,
}

impl EdgeRecord {
    pub fn to_info(&self, healthy_threshold: Duration) -> EdgeInfo {
        let age = Utc::now()
            .signed_duration_since(self.last_seen)
            .to_std()
            .unwrap_or(Duration::MAX);
        EdgeInfo {
            id: self.edge_id.clone(),
            url: self.url.clone(),
            sandbox_count: self.sandbox_count,
            healthy: age < healthy_threshold,
            last_seen: self.last_seen,
        }
    }
}

pub struct State {
    healthy_threshold: Duration,
    inner: RwLock<Inner>,
}

struct Inner {
    edges: HashMap<String, EdgeRecord>,
    sandbox_to_edge: HashMap<String, String>,
}

impl State {
    pub fn new(healthy_threshold: Duration) -> Self {
        Self {
            healthy_threshold,
            inner: RwLock::new(Inner {
                edges: HashMap::new(),
                sandbox_to_edge: HashMap::new(),
            }),
        }
    }

    pub fn healthy_threshold(&self) -> Duration {
        self.healthy_threshold
    }

    pub async fn register_edge(&self, edge_id: String, url: String, sandbox_count: u32) {
        let mut inner = self.inner.write().await;
        inner.edges.insert(
            edge_id.clone(),
            EdgeRecord {
                edge_id,
                url,
                sandbox_count,
                last_seen: Utc::now(),
                sandboxes: HashSet::new(),
            },
        );
    }

    pub async fn heartbeat(&self, edge_id: &str, sandbox_count: u32) -> bool {
        let mut inner = self.inner.write().await;
        if let Some(edge) = inner.edges.get_mut(edge_id) {
            edge.last_seen = Utc::now();
            edge.sandbox_count = sandbox_count;
            true
        } else {
            false
        }
    }

    pub async fn edges(&self) -> Vec<EdgeRecord> {
        self.inner.read().await.edges.values().cloned().collect()
    }

    pub async fn edge_infos(&self) -> Vec<EdgeInfo> {
        let threshold = self.healthy_threshold;
        self.inner
            .read()
            .await
            .edges
            .values()
            .map(|e| e.to_info(threshold))
            .collect()
    }

    pub async fn healthy_edges(&self) -> Vec<EdgeRecord> {
        let now = Utc::now();
        self.inner
            .read()
            .await
            .edges
            .values()
            .filter(|e| {
                now.signed_duration_since(e.last_seen)
                    .to_std()
                    .map_or(false, |d| d < self.healthy_threshold)
            })
            .cloned()
            .collect()
    }

    pub async fn record_sandbox(&self, sandbox_id: String, edge_id: &str) {
        let mut inner = self.inner.write().await;
        inner.sandbox_to_edge.insert(sandbox_id.clone(), edge_id.to_string());
        if let Some(edge) = inner.edges.get_mut(edge_id) {
            edge.sandboxes.insert(sandbox_id);
        }
    }

    pub async fn forget_sandbox(&self, sandbox_id: &str) {
        let mut inner = self.inner.write().await;
        if let Some(edge_id) = inner.sandbox_to_edge.remove(sandbox_id) {
            if let Some(edge) = inner.edges.get_mut(&edge_id) {
                edge.sandboxes.remove(sandbox_id);
            }
        }
    }

    pub async fn edge_for_sandbox(&self, sandbox_id: &str) -> Option<EdgeRecord> {
        let inner = self.inner.read().await;
        let edge_id = inner.sandbox_to_edge.get(sandbox_id)?;
        inner.edges.get(edge_id).cloned()
    }
}
