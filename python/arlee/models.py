"""Shared pydantic models exchanged between client, Apiserver, and Edge.

Mirrors `crates/arlee-models/src/lib.rs`; the two must stay in sync.
"""

from __future__ import annotations

from datetime import datetime
from enum import Enum
from typing import Any

from pydantic import BaseModel, Field


class Substrate(str, Enum):
    CONTAINER = "container"


class SandboxStatus(str, Enum):
    CREATING = "creating"
    RUNNING = "running"
    KILLED = "killed"
    FAILED = "failed"


class CommandType(str, Enum):
    EXEC = "exec"
    READ_FILE = "read_file"
    WRITE_FILE = "write_file"


# ---------------------------------------------------------------------------
# Memory / resource configuration (see docs/design/memory-limits.md)
# ---------------------------------------------------------------------------


class OnOom(str, Enum):
    """What the kernel kills when this sandbox hits its memory ceiling.

    KILL_PROCESS (default): cgroup `memory.oom.group=0`. Kernel kills
    individual processes; sandbox PID 1 survives. Suits training /
    long-lived workspaces.

    KILL_SANDBOX: cgroup `memory.oom.group=1`. Kernel atomically SIGKILLs
    every process in the cgroup; sandbox transitions to Failed. Suits
    eval / throw-away workloads.
    """

    KILL_PROCESS = "kill_process"
    KILL_SANDBOX = "kill_sandbox"


class ResourceSpec(BaseModel):
    """Per-sandbox resource configuration. All fields optional; None preserves
    the pre-memory-limits behavior (no kernel-enforced limits, zero
    scheduling reservation).

    Memory units are MiB (1024 * 1024 bytes), matching Docker's `-m 1024m`
    convention shared by E2B, verl, and Harbor.
    """

    memory_min_mb: int | None = None
    """Guaranteed memory floor in MiB. Kernel-enforced via cgroup v2
    `memory.min`; scheduler reserves this amount on the chosen Edge."""

    memory_max_mb: int | None = None
    """Hard memory ceiling in MiB. Kernel-enforced via cgroup v2
    `memory.max`; exceeding it triggers OOM kill (scope per `on_oom`)."""


class ExecTermination(str, Enum):
    """What ended a single `exec` invocation. None on ExecResult means the
    process exited on its own (any exit_code).
    """

    OOM = "oom"
    """Process killed because this sandbox exceeded its own memory_max_mb.
    Not retriable as-is; raise the ceiling or reduce workload memory use."""

    OOM_EDGE = "oom_edge"
    """Process killed by the system OOM killer due to Edge-wide memory
    pressure; this sandbox may have been well under its own max. Retriable
    by re-creating the sandbox (re-exec on the same sandbox is pointless
    — same Edge, same pressure)."""

    TIMEOUT = "timeout"
    """Killed by Arlee's exec timeout."""

    CONTAINER_DIED = "container_died"
    """Container died mid-exec for a non-OOM reason."""


class SandboxTermination(str, Enum):
    """What ended a sandbox. None on SandboxInfo means the sandbox is still
    Running. Parallel to ExecTermination but at sandbox-lifecycle scope.
    """

    USER_KILLED = "user_killed"
    """kill() was called."""

    OOM = "oom"
    """Container died from its own memory.max breach (typically
    on_oom=KILL_SANDBOX)."""

    OOM_EDGE = "oom_edge"
    """Container died from Edge-wide memory pressure. Rare because PID 1
    has oom_score_adj=-1000, but possible."""

    CONTAINER_CRASHED = "container_crashed"
    """Non-OOM container death."""


class SubstrateCapabilities(BaseModel):
    """What a substrate can express. Used by the apiserver to hard-reject
    substrate-incompatible CreateSandboxRequests with a clear 400 instead
    of silently dropping the constraint.
    """

    supports_elastic_memory: bool
    """True if substrate honors memory_min_mb != memory_max_mb. False for
    microVM/fullVM (memory is a single boot-time allocation)."""

    supports_on_oom: list[OnOom]
    """Which on_oom modes the substrate accepts."""

    supports_per_sandbox_memory: bool
    """True if memory is set per-sandbox at create time; false if it's a
    template/pool-level setting (e.g., Function Call)."""


# ---------------------------------------------------------------------------
# Requests
# ---------------------------------------------------------------------------


class CreateSandboxRequest(BaseModel):
    image: str
    substrate: Substrate = Substrate.CONTAINER
    env: dict[str, str] = Field(default_factory=dict)
    timeout: float | None = None
    resources: ResourceSpec = Field(default_factory=ResourceSpec)
    on_oom: OnOom = OnOom.KILL_PROCESS


class ExecRequest(BaseModel):
    command: str
    cwd: str | None = None
    env: dict[str, str] = Field(default_factory=dict)
    user: str | None = None
    timeout: float | None = None


class WriteFileRequest(BaseModel):
    content: bytes  # base64-encoded on the wire


class RegisterEdgeRequest(BaseModel):
    edge_id: str
    url: str
    sandbox_count: int = 0
    total_memory_mb: int = 0
    """Edge's total memory available to sandboxes in MiB."""
    reserved_memory_mb: int = 0
    """Sum of memory_min_mb across the Edge's currently running sandboxes."""


class HeartbeatRequest(BaseModel):
    sandbox_count: int
    reserved_memory_mb: int = 0


# ---------------------------------------------------------------------------
# Responses / shared entities
# ---------------------------------------------------------------------------


class ExecResult(BaseModel):
    exit_code: int
    stdout: str
    stderr: str
    stdout_truncated: bool = False
    stderr_truncated: bool = False
    terminated_by: ExecTermination | None = None
    """Reason the process did not exit on its own. None = normal exit
    (consult exit_code)."""


class SandboxInfo(BaseModel):
    id: str
    image: str
    substrate: Substrate
    status: SandboxStatus
    edge_id: str
    created_at: datetime
    killed_at: datetime | None = None
    resources: ResourceSpec = Field(default_factory=ResourceSpec)
    on_oom: OnOom = OnOom.KILL_PROCESS
    terminated_by: SandboxTermination | None = None
    """Reason the sandbox ended. None while Running."""


class EdgeInfo(BaseModel):
    id: str
    url: str
    sandbox_count: int
    healthy: bool
    last_seen: datetime
    total_memory_mb: int = 0
    reserved_memory_mb: int = 0


class EdgeCapacity(BaseModel):
    edge_id: str
    sandbox_count: int
    healthy: bool
    total_memory_mb: int = 0
    reserved_memory_mb: int = 0


class TrajectoryEntry(BaseModel):
    seq: int
    ts: datetime
    cmd: CommandType
    args: dict[str, Any]
    result: dict[str, Any]
    result_hash: str


class SandboxMetadata(BaseModel):
    sandbox_id: str
    created_at: datetime
    image: str
    image_digest: str | None = None
    substrate: Substrate
    env: dict[str, str]
    edge_id: str
    killed_at: datetime | None = None
    resources: ResourceSpec = Field(default_factory=ResourceSpec)
    on_oom: OnOom = OnOom.KILL_PROCESS
