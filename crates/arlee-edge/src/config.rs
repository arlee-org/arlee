use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct EdgeConfig {
    pub edge_id: String,
    pub apiserver: String,
    pub token: String,
    pub public_url: String,
    pub bind_addr: String,
    pub port: u16,
    pub trajectory_dir: PathBuf,
}

impl EdgeConfig {
    pub fn from_env() -> Result<Self> {
        let edge_id = env::var("ARLEE_EDGE_ID")
            .unwrap_or_else(|_| Uuid::new_v4().to_string());
        let apiserver = env::var("ARLEE_APISERVER")
            .context("ARLEE_APISERVER env var required")?;
        let token = env::var("ARLEE_TOKEN")
            .context("ARLEE_TOKEN env var required")?;
        let public_url = env::var("ARLEE_EDGE_PUBLIC_URL")
            .context("ARLEE_EDGE_PUBLIC_URL env var required")?;
        let bind_addr = env::var("ARLEE_EDGE_HOST")
            .unwrap_or_else(|_| "0.0.0.0".to_string());
        let port = env::var("ARLEE_EDGE_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8081);
        let trajectory_dir = env::var("ARLEE_TRAJECTORY_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/var/arlee/trajectories"));

        Ok(EdgeConfig {
            edge_id,
            apiserver,
            token,
            public_url,
            bind_addr,
            port,
            trajectory_dir,
        })
    }
}
