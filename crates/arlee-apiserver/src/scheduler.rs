//! Least-loaded placement: pick the healthy Edge with the smallest sandbox_count.

use crate::state::{EdgeRecord, State};

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("no healthy edges available")]
    NoEdges,
}

pub async fn pick_edge(state: &State) -> Result<EdgeRecord, SchedulerError> {
    let mut healthy = state.healthy_edges().await;
    if healthy.is_empty() {
        return Err(SchedulerError::NoEdges);
    }
    healthy.sort_by_key(|e| e.sandbox_count);
    Ok(healthy.into_iter().next().unwrap())
}
