//! Wire types shared between Apiserver, Edge, and (via JSON) the Python SDK.
//!
//! Field names mirror the Python `pydantic` models in `python/arlee/models.py`
//! and must stay in sync.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
// Memory / resource configuration (see docs/memory-limits.md)
// ---------------------------------------------------------------------------

/// Per-sandbox resource configuration. All fields optional; None preserves the
/// pre-memory-limits behavior (no kernel-enforced limits, zero scheduling
/// reservation).
///
/// Memory units are MiB (1024 * 1024 bytes), matching Docker's `-m 1024m`
/// convention shared by E2B, verl, and Harbor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceSpec {
    /// Guaranteed memory floor in MiB. Kernel-enforced via cgroup v2
    /// `memory.min`; scheduler reserves this amount on the chosen Edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_min_mb: Option<u32>,
    /// Hard memory ceiling in MiB. Kernel-enforced via cgroup v2 `memory.max`;
    /// exceeding it triggers OOM kill (scope per [`OnOom`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max_mb: Option<u32>,
}

/// What the kernel kills when this sandbox hits its `memory.max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnOom {
    /// Default. cgroup `memory.oom.group=0`: kernel kills individual processes;
    /// sandbox PID 1 (with oom_score_adj=-1000) survives so subsequent execs
    /// against the same sandbox still work.
    KillProcess,
    /// cgroup `memory.oom.group=1`: kernel atomically SIGKILLs every process
    /// in the cgroup; sandbox transitions to Failed and subsequent operations
    /// error out.
    KillSandbox,
}

impl Default for OnOom {
    fn default() -> Self {
        Self::KillProcess
    }
}

/// What ended a single `exec` invocation. `None` on [`ExecResult`] means the
/// process exited on its own (any exit_code).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecTermination {
    /// Process killed because this sandbox exceeded its own `memory_max_mb`.
    /// Not retriable as-is; raise the ceiling or reduce workload memory use.
    Oom,
    /// Process killed by the system OOM killer due to Edge-wide memory
    /// pressure; this sandbox may have been well under its own max.
    /// Retriable by re-creating the sandbox (re-exec on the same sandbox is
    /// pointless — it's on the same Edge under the same pressure).
    OomEdge,
    /// Killed by Arlee's exec timeout.
    Timeout,
    /// Container died mid-exec for a non-OOM reason.
    ContainerDied,
}

/// What ended a sandbox. `None` on [`SandboxInfo`] means the sandbox is still
/// Running. Parallel to [`ExecTermination`] but scoped to the sandbox lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxTermination {
    /// `kill()` was called.
    UserKilled,
    /// Container died from its own `memory.max` breach (typically
    /// `on_oom=KillSandbox`).
    Oom,
    /// Container died from Edge-wide memory pressure. Rare because PID 1 has
    /// oom_score_adj=-1000 (immune from global OOM killer), but possible.
    OomEdge,
    /// Non-OOM container death.
    ContainerCrashed,
}

/// What a substrate can express. Used by the apiserver to hard-reject
/// substrate-incompatible CreateSandboxRequests with a clear 400 instead of
/// silently dropping the constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubstrateCapabilities {
    /// Does the substrate distinguish `memory_min_mb != memory_max_mb`?
    /// True for Container (cgroup v2 memory.min vs memory.max); false for
    /// microVM/fullVM where memory is a single boot-time allocation.
    pub supports_elastic_memory: bool,
    /// Which [`OnOom`] modes are accepted.
    pub supports_on_oom: HashSet<OnOom>,
    /// True if memory is set per-sandbox at create time; false if it's a
    /// template/pool-level setting (e.g., Function Call).
    pub supports_per_sandbox_memory: bool,
}

impl SubstrateCapabilities {
    /// Capabilities for [`Substrate::Container`].
    pub fn for_container() -> Self {
        let mut supports_on_oom = HashSet::new();
        supports_on_oom.insert(OnOom::KillProcess);
        supports_on_oom.insert(OnOom::KillSandbox);
        Self {
            supports_elastic_memory: true,
            supports_on_oom,
            supports_per_sandbox_memory: true,
        }
    }

    /// Lookup table keyed by substrate. The apiserver uses this for request
    /// validation; each substrate implementation also exposes the same via
    /// `SubstrateRuntime::capabilities`.
    pub fn for_substrate(s: Substrate) -> Self {
        match s {
            Substrate::Container => Self::for_container(),
        }
    }
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
    #[serde(default)]
    pub resources: ResourceSpec,
    #[serde(default)]
    pub on_oom: OnOom,
}

fn default_substrate() -> Substrate {
    Substrate::Container
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub user: Option<String>,
    pub timeout: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterEdgeRequest {
    pub edge_id: String,
    pub url: String,
    #[serde(default)]
    pub sandbox_count: u32,
    /// Edge's total memory available to sandboxes in MiB (from /proc/meminfo
    /// minus a system reserve). Reported once at registration; the apiserver
    /// uses this as the denominator for spread-by-ratio scheduling.
    #[serde(default)]
    pub total_memory_mb: u32,
    /// Sum of memory_min_mb across the Edge's currently running sandboxes.
    #[serde(default)]
    pub reserved_memory_mb: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub sandbox_count: u32,
    /// Updated reservation total; reconciles the apiserver's optimistic count.
    #[serde(default)]
    pub reserved_memory_mb: u32,
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
    /// Reason the process did not exit on its own. `None` means a normal exit
    /// (consult `exit_code`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminated_by: Option<ExecTermination>,
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
    #[serde(default)]
    pub resources: ResourceSpec,
    #[serde(default)]
    pub on_oom: OnOom,
    /// Reason the sandbox ended. `None` while Running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminated_by: Option<SandboxTermination>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeInfo {
    pub id: String,
    pub url: String,
    pub sandbox_count: u32,
    pub healthy: bool,
    pub last_seen: DateTime<Utc>,
    #[serde(default)]
    pub total_memory_mb: u32,
    #[serde(default)]
    pub reserved_memory_mb: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeCapacity {
    pub edge_id: String,
    pub sandbox_count: u32,
    pub healthy: bool,
    #[serde(default)]
    pub total_memory_mb: u32,
    #[serde(default)]
    pub reserved_memory_mb: u32,
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
    #[serde(default)]
    pub resources: ResourceSpec,
    #[serde(default)]
    pub on_oom: OnOom,
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
