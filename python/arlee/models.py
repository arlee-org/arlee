"""Shared pydantic models exchanged between client, Apiserver, and Edge."""

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
# Requests
# ---------------------------------------------------------------------------


class CreateSandboxRequest(BaseModel):
    image: str
    substrate: Substrate = Substrate.CONTAINER
    env: dict[str, str] = Field(default_factory=dict)
    timeout: float | None = None


class ExecRequest(BaseModel):
    command: str
    timeout: float | None = None


class WriteFileRequest(BaseModel):
    content: bytes  # base64-encoded on the wire


class RegisterEdgeRequest(BaseModel):
    edge_id: str
    url: str
    sandbox_count: int = 0


class HeartbeatRequest(BaseModel):
    sandbox_count: int


# ---------------------------------------------------------------------------
# Responses / shared entities
# ---------------------------------------------------------------------------


class ExecResult(BaseModel):
    exit_code: int
    stdout: str
    stderr: str
    stdout_truncated: bool = False
    stderr_truncated: bool = False


class SandboxInfo(BaseModel):
    id: str
    image: str
    substrate: Substrate
    status: SandboxStatus
    edge_id: str
    created_at: datetime
    killed_at: datetime | None = None


class EdgeInfo(BaseModel):
    id: str
    url: str
    sandbox_count: int
    healthy: bool
    last_seen: datetime


class EdgeCapacity(BaseModel):
    edge_id: str
    sandbox_count: int
    healthy: bool


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
