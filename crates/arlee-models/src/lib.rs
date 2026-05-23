//! Wire types shared between Apiserver, Edge, and (via JSON) the Python SDK.
//!
//! Field names mirror the Python `pydantic` models in `python/arlee/models.py`
//! and must stay in sync.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Substrate {
    Container,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxStatus {
    Creating,
    Running,
    Killed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandType {
    Exec,
    ReadFile,
    WriteFile,
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSandboxRequest {
    pub image: String,
    #[serde(default = "default_substrate")]
    pub substrate: Substrate,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub timeout: Option<f64>,
}

fn default_substrate() -> Substrate {
    Substrate::Container
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    pub command: String,
    pub timeout: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterEdgeRequest {
    pub edge_id: String,
    pub url: String,
    #[serde(default)]
    pub sandbox_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub sandbox_count: u32,
}

// ---------------------------------------------------------------------------
// Responses / shared entities
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    #[serde(default)]
    pub stdout_truncated: bool,
    #[serde(default)]
    pub stderr_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxInfo {
    pub id: String,
    pub image: String,
    pub substrate: Substrate,
    pub status: SandboxStatus,
    pub edge_id: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub killed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeInfo {
    pub id: String,
    pub url: String,
    pub sandbox_count: u32,
    pub healthy: bool,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeCapacity {
    pub edge_id: String,
    pub sandbox_count: u32,
    pub healthy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryEntry {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub cmd: CommandType,
    pub args: serde_json::Value,
    pub result: serde_json::Value,
    pub result_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxMetadata {
    pub sandbox_id: String,
    pub created_at: DateTime<Utc>,
    pub image: String,
    #[serde(default)]
    pub image_digest: Option<String>,
    pub substrate: Substrate,
    pub env: HashMap<String, String>,
    pub edge_id: String,
    #[serde(default)]
    pub killed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OkResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl OkResponse {
    pub fn ok() -> Self {
        Self { ok: true, size: None }
    }

    pub fn with_size(size: u64) -> Self {
        Self { ok: true, size: Some(size) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub edge_count: u32,
    pub healthy_edges: u32,
}
