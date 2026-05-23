"""Arlee — Agentic RL Execution Environment Python SDK."""

from arlee.client import Client
from arlee.models import (
    EdgeInfo,
    SandboxInfo,
    SandboxStatus,
    Substrate,
    TrajectoryEntry,
)

__version__ = "0.1.0"

_default_client: Client | None = None


def _client() -> Client:
    global _default_client
    if _default_client is None:
        _default_client = Client.from_env()
    return _default_client


async def create_sandbox(**kwargs) -> SandboxInfo:
    return await _client().create_sandbox(**kwargs)


async def kill_sandbox(sandbox_id: str) -> None:
    return await _client().kill_sandbox(sandbox_id)


async def exec(sandbox_id: str, command: str, timeout: float | None = None):
    return await _client().exec(sandbox_id, command, timeout)


async def read_file(sandbox_id: str, path: str) -> bytes:
    return await _client().read_file(sandbox_id, path)


async def write_file(sandbox_id: str, path: str, content: bytes) -> None:
    return await _client().write_file(sandbox_id, path, content)


async def get_trajectory(sandbox_id: str) -> list[TrajectoryEntry]:
    return await _client().get_trajectory(sandbox_id)


async def list_sandboxes() -> list[SandboxInfo]:
    return await _client().list_sandboxes()


async def list_edges() -> list[EdgeInfo]:
    return await _client().list_edges()


__all__ = [
    "Client",
    "EdgeInfo",
    "SandboxInfo",
    "SandboxStatus",
    "Substrate",
    "TrajectoryEntry",
    "create_sandbox",
    "exec",
    "get_trajectory",
    "kill_sandbox",
    "list_edges",
    "list_sandboxes",
    "read_file",
    "write_file",
]
