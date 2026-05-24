mod api;
mod config;
mod error;
mod state;

use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::api::AppState;
use crate::config::ApiserverConfig;
use crate::state::State as ClusterState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = Arc::new(ApiserverConfig::from_env()?);
    let cluster = Arc::new(ClusterState::new(cfg.edge_healthy));
    let http = reqwest::Client::builder()
        .timeout(cfg.edge_request_timeout)
        .build()?;

    let state = Arc::new(AppState {
        state: cluster,
        cfg: cfg.clone(),
        http,
    });
    let app = api::router(state);

    let addr = format!("{}:{}", cfg.bind_addr, cfg.port);
    let listener = TcpListener::bind(&addr).await?;
    info!(%addr, "arlee-apiserver listening");
    axum::serve(listener, app).await?;
    Ok(())
}
