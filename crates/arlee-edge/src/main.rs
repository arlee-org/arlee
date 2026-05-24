mod api;
mod config;
mod docker_substrate;
mod edge_cgroup;
mod error;
mod substrate;
mod trajectory;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use arlee_models::{HeartbeatRequest, RegisterEdgeRequest};
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use api::AppState;
use config::EdgeConfig;
use docker_substrate::DockerSubstrate;
use substrate::SubstrateRuntime;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = EdgeConfig::from_env()?;
    info!(edge_id = %cfg.edge_id, "starting arlee-edge");

    let docker_substrate =
        DockerSubstrate::new(cfg.edge_id.clone(), cfg.trajectory_dir.clone()).await?;
    match docker_substrate.reconcile_stale_cgroups() {
        Ok(0) => {}
        Ok(n) => info!(cleaned = n, "reconciled stale cgroups at startup"),
        Err(e) => warn!("cgroup reconciliation: {e}"),
    }
    let runner: Arc<dyn SubstrateRuntime> = Arc::new(docker_substrate);
    info!(
        capabilities = ?runner.capabilities(),
        total_memory_mb = runner.total_memory_mb(),
        "substrate ready"
    );

    // Spawn registration + heartbeat loop.
    let reg_cfg = cfg.clone();
    let reg_runner = runner.clone();
    tokio::spawn(async move {
        if let Err(e) = register_loop(reg_cfg, reg_runner).await {
            warn!("register loop terminated: {e}");
        }
    });

    let state = Arc::new(AppState {
        runner,
        token: cfg.token.clone(),
        edge_id: cfg.edge_id.clone(),
    });
    let app = api::router(state);

    let addr = format!("{}:{}", cfg.bind_addr, cfg.port);
    let listener = TcpListener::bind(&addr).await?;
    info!(%addr, "listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn register_loop(cfg: EdgeConfig, runner: Arc<dyn SubstrateRuntime>) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let register_url = format!("{}/edges/register", cfg.apiserver.trim_end_matches('/'));
    let heartbeat_url = format!(
        "{}/edges/{}/heartbeat",
        cfg.apiserver.trim_end_matches('/'),
        cfg.edge_id
    );

    // Initial register loop with backoff.
    loop {
        let body = RegisterEdgeRequest {
            edge_id: cfg.edge_id.clone(),
            url: cfg.public_url.clone(),
            sandbox_count: runner.sandbox_count().await,
            total_memory_mb: runner.total_memory_mb(),
            reserved_memory_mb: runner.reserved_memory_mb().await,
        };
        let res = http
            .post(&register_url)
            .header("X-Arlee-Token", &cfg.token)
            .json(&body)
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {
                info!("registered with apiserver");
                break;
            }
            Ok(r) => warn!("register failed with status {}", r.status()),
            Err(e) => warn!("register failed: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // Heartbeat loop.
    loop {
        tokio::time::sleep(HEARTBEAT_INTERVAL).await;
        let body = HeartbeatRequest {
            sandbox_count: runner.sandbox_count().await,
            reserved_memory_mb: runner.reserved_memory_mb().await,
        };
        let res = http
            .post(&heartbeat_url)
            .header("X-Arlee-Token", &cfg.token)
            .json(&body)
            .send()
            .await;
        match res {
            Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {
                warn!("apiserver returned 404 on heartbeat; re-registering");
                let reg = RegisterEdgeRequest {
                    edge_id: cfg.edge_id.clone(),
                    url: cfg.public_url.clone(),
                    sandbox_count: runner.sandbox_count().await,
                    total_memory_mb: runner.total_memory_mb(),
                    reserved_memory_mb: runner.reserved_memory_mb().await,
                };
                let _ = http
                    .post(&register_url)
                    .header("X-Arlee-Token", &cfg.token)
                    .json(&reg)
                    .send()
                    .await;
            }
            Ok(r) if !r.status().is_success() => {
                warn!("heartbeat failed with status {}", r.status());
            }
            Err(e) => warn!("heartbeat error: {e}"),
            _ => {}
        }
    }
}
