"""Arlee Python SDK.

Primary usage — module-level + Sandbox-as-object + async-context auto-kill:

    import arlee

    async with arlee.create_sandbox(image="ubuntu:22.04") as sb:
        res = await sb.exec("echo hello")
        await sb.write_file("/tmp/file", b"data")
        contents = await sb.read_file("/tmp/file")
        trajectory = await sb.get_trajectory()
    # sb.kill() runs on context exit

Configuration via env vars (`ARLEE_APISERVER` + `ARLEE_TOKEN`) by default;
override with `arlee.configure(apiserver=..., token=...)` before the first
SDK call. For multi-cluster or testing scenarios, construct `Client` directly:

    async with arlee.Client(apiserver=..., token=...) as c:
        sb = await c.create_sandbox(...)
"""

from __future__ import annotations

import os

from arlee.client import Client
from arlee.models import (
    EdgeCapacity,
    EdgeInfo,
    ExecResult,
    ExecTermination,
    OnOom,
    ResourceSpec,
    SandboxInfo,
    SandboxStatus,
    SandboxTermination,
    Substrate,
    TrajectoryEntry,
)
from arlee.sandbox import Sandbox

__version__ = "0.1.0"

_default_client: Client | None = None
_configured_apiserver: str | None = None
_configured_token: str | None = None
_configured_timeout: float | None = None


def configure(
    *,
    apiserver: str | None = None,
    token: str | None = None,
    timeout: float | None = None,
) -> None:
    """Override the default client config. Must be called before any
    module-level SDK call. Env vars `ARLEE_APISERVER` + `ARLEE_TOKEN` are
    used as fallback if `apiserver` / `token` are omitted; `timeout` falls
    back to the Client default (300s)."""
    global _configured_apiserver, _configured_token, _configured_timeout
    global _default_client
    if _default_client is not None:
        raise RuntimeError(
            "arlee.configure() must be called before the first SDK call"
        )
    if apiserver is not None:
        _configured_apiserver = apiserver
    if token is not None:
        _configured_token = token
    if timeout is not None:
        _configured_timeout = timeout


def _client() -> Client:
    global _default_client
    if _default_client is None:
        apiserver = _configured_apiserver or os.environ.get("ARLEE_APISERVER")
        token = _configured_token or os.environ.get("ARLEE_TOKEN")
        if not apiserver or not token:
            raise RuntimeError(
                "Arlee SDK requires ARLEE_APISERVER + ARLEE_TOKEN env vars, "
                "or call arlee.configure(apiserver=..., token=...) first"
            )
        kwargs: dict = {"apiserver": apiserver, "token": token}
        if _configured_timeout is not None:
            kwargs["timeout"] = _configured_timeout
        _default_client = Client(**kwargs)
    return _default_client


# ---------------------------------------------------------------------------
# Module-level entry points (single-cluster, common case)
# ---------------------------------------------------------------------------


async def create_sandbox(
    image: str,
    substrate: Substrate | str = Substrate.CONTAINER,
    env: dict[str, str] | None = None,
    timeout: float | None = None,
    *,
    memory_min_mb: int | None = None,
    memory_max_mb: int | None = None,
    on_oom: OnOom | str = OnOom.KILL_PROCESS,
) -> Sandbox:
    """Module-level shortcut for `Client.create_sandbox`. See that method
    for the full docstring (memory limits, on_oom semantics, OOM
    interpretation)."""
    return await _client().create_sandbox(
        image=image,
        substrate=substrate,
        env=env,
        timeout=timeout,
        memory_min_mb=memory_min_mb,
        memory_max_mb=memory_max_mb,
        on_oom=on_oom,
    )


async def list_sandboxes() -> list[SandboxInfo]:
    return await _client().list_sandboxes()


async def list_edges() -> list[EdgeInfo]:
    return await _client().list_edges()


async def capacity() -> list[EdgeCapacity]:
    return await _client().capacity()


async def health() -> dict:
    return await _client().health()


__all__ = [
    "Client",
    "EdgeCapacity",
    "EdgeInfo",
    "ExecResult",
    "ExecTermination",
    "OnOom",
    "ResourceSpec",
    "Sandbox",
    "SandboxInfo",
    "SandboxStatus",
    "SandboxTermination",
    "Substrate",
    "TrajectoryEntry",
    "capacity",
    "configure",
    "create_sandbox",
    "health",
    "list_edges",
    "list_sandboxes",
]
