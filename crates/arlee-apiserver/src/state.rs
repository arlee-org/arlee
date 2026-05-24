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
    pub total_memory_mb: u32,
    pub reserved_memory_mb: u32,
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
            total_memory_mb: self.total_memory_mb,
            reserved_memory_mb: self.reserved_memory_mb,
        }
    }

    fn available_memory_mb(&self) -> u32 {
        self.total_memory_mb.saturating_sub(self.reserved_memory_mb)
    }
}

pub struct State {
    healthy_threshold: Duration,
    inner: RwLock<Inner>,
}

struct Inner {
    edges: HashMap<String, EdgeRecord>,
    sandbox_to_edge: HashMap<String, String>,
    /// Per-sandbox memory_min_mb the apiserver reserved at pick time. Needed
    /// so `forget_sandbox` can decrement the Edge's reserved_memory_mb by the
    /// right amount; without this the apiserver's view drifts upward each
    /// time a sandbox is killed.
    sandbox_min_mb: HashMap<String, u32>,
}

impl State {
    pub fn new(healthy_threshold: Duration) -> Self {
        Self {
            healthy_threshold,
            inner: RwLock::new(Inner {
                edges: HashMap::new(),
                sandbox_to_edge: HashMap::new(),
                sandbox_min_mb: HashMap::new(),
            }),
        }
    }

    pub fn healthy_threshold(&self) -> Duration {
        self.healthy_threshold
    }

    pub async fn register_edge(
        &self,
        edge_id: String,
        url: String,
        sandbox_count: u32,
        total_memory_mb: u32,
        reserved_memory_mb: u32,
    ) {
        let mut inner = self.inner.write().await;
        inner.edges.insert(
            edge_id.clone(),
            EdgeRecord {
                edge_id,
                url,
                sandbox_count,
                last_seen: Utc::now(),
                sandboxes: HashSet::new(),
                total_memory_mb,
                reserved_memory_mb,
            },
        );
    }

    pub async fn heartbeat(
        &self,
        edge_id: &str,
        sandbox_count: u32,
        reserved_memory_mb: u32,
    ) -> bool {
        let mut inner = self.inner.write().await;
        if let Some(edge) = inner.edges.get_mut(edge_id) {
            edge.last_seen = Utc::now();
            edge.sandbox_count = sandbox_count;
            // Take max(apiserver_value, edge_reported). Apiserver's value can
            // be temporarily higher than the Edge's because of the
            // pick-then-forward race: pick_with_memory increments
            // optimistically; an Edge heartbeat that fires before the Edge
            // has processed the forwarded create reports the pre-create
            // value, which would otherwise clobber our optimistic count. The
            // forget_sandbox path is the only authoritative way to decrement
            // — heartbeat is allowed to raise above (drift catch) but never
            // lower.
            edge.reserved_memory_mb = std::cmp::max(edge.reserved_memory_mb, reserved_memory_mb);
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

    /// Pick a healthy Edge that has at least `request_min_mb` MiB of
    /// unreserved memory; among those, prefer the one with the highest
    /// available-memory ratio (spread, not pack). Tiebreak by lowest
    /// sandbox_count.
    ///
    /// Returns `Some((edge, NoCapacityReason::None))` if a placement was made,
    /// `None` if no healthy Edge satisfies the requirement.
    ///
    /// Atomically increments both `sandbox_count` and `reserved_memory_mb` on
    /// the chosen Edge under the write lock so concurrent picks distribute
    /// rather than stack. The caller MUST call [`release_reservation`] (with
    /// the same `request_min_mb`) if the downstream create_sandbox call to
    /// that Edge fails. The next heartbeat reconciles the optimistic numbers
    /// against the Edge's authoritative view.
    pub async fn pick_with_memory(&self, request_min_mb: u32) -> PickResult {
        let mut inner = self.inner.write().await;
        let now = Utc::now();
        let threshold = self.healthy_threshold;

        // Partition: which healthy edges can fit this request?
        let mut feasible: Vec<(String, f64, u32)> = inner
            .edges
            .values()
            .filter(|e| {
                now.signed_duration_since(e.last_seen)
                    .to_std()
                    .map_or(false, |d| d < threshold)
            })
            .filter(|e| e.available_memory_mb() >= request_min_mb)
            .map(|e| {
                // Score: post-placement available ratio. Higher = emptier
                // after placement, which is what spread wants.
                // For Edges with total_memory_mb=0 (legacy or misreporting
                // Edges), we fall back to the count-based tiebreaker by
                // treating their score as a sentinel.
                let score = if e.total_memory_mb > 0 {
                    let post = e.total_memory_mb.saturating_sub(
                        e.reserved_memory_mb.saturating_add(request_min_mb),
                    );
                    (post as f64) / (e.total_memory_mb as f64)
                } else {
                    0.0
                };
                (e.edge_id.clone(), score, e.sandbox_count)
            })
            .collect();

        if feasible.is_empty() {
            // Distinguish "no edges at all (healthy)" from "edges exist but
            // none fits". Caller maps to NoEdges vs NoCapacity respectively.
            let any_healthy = inner.edges.values().any(|e| {
                now.signed_duration_since(e.last_seen)
                    .to_std()
                    .map_or(false, |d| d < threshold)
            });
            return if any_healthy {
                PickResult::NoCapacity
            } else {
                PickResult::NoEdges
            };
        }

        // Best score first; tiebreak by lowest sandbox_count.
        feasible.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.2.cmp(&b.2))
        });
        let chosen_id = feasible[0].0.clone();

        let edge = match inner.edges.get_mut(&chosen_id) {
            Some(e) => e,
            None => return PickResult::NoEdges, // race; should not happen under write lock
        };
        edge.sandbox_count = edge.sandbox_count.saturating_add(1);
        edge.reserved_memory_mb = edge.reserved_memory_mb.saturating_add(request_min_mb);
        PickResult::Ok(edge.clone())
    }

    /// Roll back the optimistic increments from `pick_with_memory` when the
    /// downstream create_sandbox call fails. Must be called with the same
    /// `request_min_mb` that was passed to pick.
    pub async fn release_reservation(&self, edge_id: &str, request_min_mb: u32) {
        let mut inner = self.inner.write().await;
        if let Some(e) = inner.edges.get_mut(edge_id) {
            e.sandbox_count = e.sandbox_count.saturating_sub(1);
            e.reserved_memory_mb = e.reserved_memory_mb.saturating_sub(request_min_mb);
        }
    }

    pub async fn record_sandbox(&self, sandbox_id: String, edge_id: &str, memory_min_mb: u32) {
        let mut inner = self.inner.write().await;
        inner.sandbox_to_edge.insert(sandbox_id.clone(), edge_id.to_string());
        inner.sandbox_min_mb.insert(sandbox_id.clone(), memory_min_mb);
        if let Some(edge) = inner.edges.get_mut(edge_id) {
            edge.sandboxes.insert(sandbox_id);
        }
    }

    pub async fn forget_sandbox(&self, sandbox_id: &str) {
        let mut inner = self.inner.write().await;
        let min_mb = inner.sandbox_min_mb.remove(sandbox_id).unwrap_or(0);
        if let Some(edge_id) = inner.sandbox_to_edge.remove(sandbox_id) {
            if let Some(edge) = inner.edges.get_mut(&edge_id) {
                edge.sandboxes.remove(sandbox_id);
                // Decrement reserved_memory_mb — heartbeat takes max() so it
                // would otherwise stay at the pre-kill level until the Edge
                // catches up. sandbox_count is intentionally NOT decremented
                // here: the heartbeat overwrites it from the Edge's
                // authoritative view, so doing both would race.
                edge.reserved_memory_mb = edge.reserved_memory_mb.saturating_sub(min_mb);
            }
        }
    }

    pub async fn edge_for_sandbox(&self, sandbox_id: &str) -> Option<EdgeRecord> {
        let inner = self.inner.read().await;
        let edge_id = inner.sandbox_to_edge.get(sandbox_id)?;
        inner.edges.get(edge_id).cloned()
    }
}

#[derive(Debug)]
pub enum PickResult {
    Ok(EdgeRecord),
    NoEdges,
    NoCapacity,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> State {
        State::new(Duration::from_secs(30))
    }

    async fn add_edge(state: &State, id: &str, total: u32, reserved: u32, count: u32) {
        state
            .register_edge(id.to_string(), format!("http://{id}"), count, total, reserved)
            .await;
        // register_edge sets last_seen=now, so the edge is healthy
    }

    #[tokio::test]
    async fn no_edges_returns_no_edges() {
        let s = make_state();
        assert!(matches!(s.pick_with_memory(1024).await, PickResult::NoEdges));
    }

    #[tokio::test]
    async fn no_capacity_returns_no_capacity() {
        let s = make_state();
        add_edge(&s, "e1", 4096, 4000, 1).await;
        // request needs 1024 but only 96 MiB free
        assert!(matches!(s.pick_with_memory(1024).await, PickResult::NoCapacity));
    }

    #[tokio::test]
    async fn picks_edge_with_highest_available_ratio() {
        let s = make_state();
        // e1: 16 GiB total, 12 GiB reserved → 4 GiB free → 25% available
        add_edge(&s, "e1", 16384, 12288, 6).await;
        // e2: 16 GiB total,  4 GiB reserved → 12 GiB free → 75% available
        add_edge(&s, "e2", 16384, 4096, 2).await;
        // request 2 GiB: both feasible, e2 wins by ratio
        match s.pick_with_memory(2048).await {
            PickResult::Ok(e) => assert_eq!(e.edge_id, "e2"),
            other => panic!("expected Ok(e2), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn tiebreaks_by_sandbox_count_when_scores_match() {
        let s = make_state();
        // Both identical except sandbox_count
        add_edge(&s, "e1", 16384, 4096, 3).await;
        add_edge(&s, "e2", 16384, 4096, 1).await;
        match s.pick_with_memory(2048).await {
            PickResult::Ok(e) => assert_eq!(e.edge_id, "e2"), // lower count wins
            other => panic!("expected Ok(e2), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn atomic_reservation_distributes_concurrent_picks() {
        let s = make_state();
        // Single empty Edge with enough room for 3 sandboxes of 4 GiB each
        add_edge(&s, "e1", 16384, 0, 0).await;
        // After first pick: reserved=4096
        let _ = s.pick_with_memory(4096).await;
        let edges = s.edges().await;
        let e1 = edges.iter().find(|e| e.edge_id == "e1").unwrap();
        assert_eq!(e1.reserved_memory_mb, 4096);
        assert_eq!(e1.sandbox_count, 1);
        // Second pick
        let _ = s.pick_with_memory(4096).await;
        let edges = s.edges().await;
        let e1 = edges.iter().find(|e| e.edge_id == "e1").unwrap();
        assert_eq!(e1.reserved_memory_mb, 8192);
        assert_eq!(e1.sandbox_count, 2);
    }

    #[tokio::test]
    async fn release_reservation_rolls_back_both_counters() {
        let s = make_state();
        add_edge(&s, "e1", 16384, 0, 0).await;
        let _ = s.pick_with_memory(4096).await;
        s.release_reservation("e1", 4096).await;
        let edges = s.edges().await;
        let e1 = edges.iter().find(|e| e.edge_id == "e1").unwrap();
        assert_eq!(e1.reserved_memory_mb, 0);
        assert_eq!(e1.sandbox_count, 0);
    }

    #[tokio::test]
    async fn zero_request_min_always_feasible() {
        // Callers that pass memory_min_mb=None should map to 0 and always
        // be placeable as long as a healthy edge exists.
        let s = make_state();
        add_edge(&s, "e1", 16384, 16384, 10).await; // fully reserved
        match s.pick_with_memory(0).await {
            PickResult::Ok(e) => assert_eq!(e.edge_id, "e1"),
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn legacy_edge_with_zero_total_memory_still_placeable() {
        // Edges that don't report total_memory_mb (e.g., pre-upgrade) should
        // still be usable for requests with no memory_min_mb.
        let s = make_state();
        add_edge(&s, "e1", 0, 0, 0).await;
        match s.pick_with_memory(0).await {
            PickResult::Ok(e) => assert_eq!(e.edge_id, "e1"),
            other => panic!("expected Ok, got {:?}", other),
        }
    }
}
