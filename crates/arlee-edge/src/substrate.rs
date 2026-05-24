//! Substrate runtime abstraction.
//!
//! The wire protocol treats `Substrate` as a first-class field on
//! [`CreateSandboxRequest`]; the apiserver validates per-substrate
//! [`SubstrateCapabilities`]. This trait is the Rust-layer mirror: each
//! substrate (Container today; microVM / fullVM / Function Call in the
//! future per [`docs/dsec.md`](../../../docs/dsec.md)) implements
//! `SubstrateRuntime` once. The Edge stores one as `Arc<dyn SubstrateRuntime>`
//! and dispatches sandbox operations through it.
//!
//! Rationale for introducing the trait now (only one impl exists today) is
//! captured in `docs/memory-limits.md` §4.1.
//!
//! Adding a new substrate is: one new `impl SubstrateRuntime` + one new
//! variant in the [`Substrate`] enum + a matching
//! `SubstrateCapabilities::for_<name>()` preset. No trait churn, no apiserver
//! change.

use anyhow::Result;
use async_trait::async_trait;

use arlee_models::{
    CreateSandboxRequest, ExecRequest, ExecResult, SandboxInfo, SubstrateCapabilities,
};

#[async_trait]
pub trait SubstrateRuntime: Send + Sync {
    /// What this substrate can express. Returned by reference so callers can
    /// hold a borrowed view; the value is constant for the substrate's
    /// lifetime.
    fn capabilities(&self) -> &SubstrateCapabilities;

    /// Edge's total memory budget in MiB available to sandboxes. Reported once
    /// to the apiserver at registration; constant for the process lifetime.
    fn total_memory_mb(&self) -> u32;

    // ----- lifecycle -----

    async fn create(&self, req: &CreateSandboxRequest) -> Result<SandboxInfo>;
    async fn kill(&self, sandbox_id: &str) -> Result<()>;

    // ----- sandbox operations -----

    async fn exec(&self, sandbox_id: &str, req: &ExecRequest) -> Result<ExecResult>;
    async fn read_file(&self, sandbox_id: &str, path: &str) -> Result<Vec<u8>>;
    async fn write_file(&self, sandbox_id: &str, path: &str, content: Vec<u8>) -> Result<()>;
    async fn get_trajectory(&self, sandbox_id: &str) -> Result<Vec<serde_json::Value>>;

    // ----- introspection (for heartbeats and capacity endpoints) -----

    async fn list_infos(&self) -> Vec<SandboxInfo>;
    async fn sandbox_count(&self) -> u32;
    /// Sum of `memory_min_mb` across currently running sandboxes; the
    /// apiserver scheduler uses this to enforce admission.
    async fn reserved_memory_mb(&self) -> u32;
}
