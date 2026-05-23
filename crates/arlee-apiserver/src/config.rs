use std::env;
use std::time::Duration;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct ApiserverConfig {
    pub token: String,
    pub bind_addr: String,
    pub port: u16,
    pub edge_healthy: Duration,
    pub edge_request_timeout: Duration,
}

impl ApiserverConfig {
    pub fn from_env() -> Result<Self> {
        let token = env::var("ARLEE_TOKEN").context("ARLEE_TOKEN env var required")?;
        let bind_addr =
            env::var("ARLEE_APISERVER_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
        let port = env::var("ARLEE_APISERVER_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8080);
        let edge_healthy_s = env::var("ARLEE_EDGE_HEALTHY_SECONDS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(30.0);
        let edge_request_timeout_s = env::var("ARLEE_EDGE_REQUEST_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(300.0);
        Ok(Self {
            token,
            bind_addr,
            port,
            edge_healthy: Duration::from_secs_f64(edge_healthy_s),
            edge_request_timeout: Duration::from_secs_f64(edge_request_timeout_s),
        })
    }
}
