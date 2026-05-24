"""Arlee Python SDK — async HTTP client against Apiserver.

`Client` holds the httpx connection pool + auth token; per-sandbox operations
are reached through the `Sandbox` handle returned by `create_sandbox`. Most
users go through the module-level helpers (`arlee.create_sandbox(...)`) and
never construct Client explicitly.
"""

from __future__ import annotations

import os
from types import TracebackType

import httpx

from arlee.models import (
    CreateSandboxRequest,
    EdgeCapacity,
    EdgeInfo,
    ExecRequest,
    ExecResult,
    OnOom,
    ResourceSpec,
    SandboxInfo,
    Substrate,
    TrajectoryEntry,
)
from arlee.sandbox import Sandbox


class Client:
    def __init__(self, apiserver: str, token: str, timeout: float = 300.0):
        self._http = httpx.AsyncClient(
            base_url=apiserver.rstrip("/"),
            headers={"X-Arlee-Token": token},
            timeout=timeout,
        )

    @classmethod
    def from_env(cls) -> Client:
        url = os.environ.get("ARLEE_APISERVER")
        if not url:
            raise RuntimeError("ARLEE_APISERVER env var not set")
        token = os.environ.get("ARLEE_TOKEN")
        if not token:
            raise RuntimeError("ARLEE_TOKEN env var not set")
        return cls(apiserver=url, token=token)

    async def __aenter__(self) -> Client:
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        await self.aclose()

    async def aclose(self) -> None:
        await self._http.aclose()

    # ------------------------------------------------------------------
    # Public surface: sandbox factory + cluster introspection
    # ------------------------------------------------------------------

    async def create_sandbox(
        self,
        image: str,
        substrate: Substrate | str = Substrate.CONTAINER,
        env: dict[str, str] | None = None,
        timeout: float | None = None,
        *,
        memory_min_mb: int | None = None,
        memory_max_mb: int | None = None,
        on_oom: OnOom | str = OnOom.KILL_PROCESS,
    ) -> Sandbox:
        """Create a sandbox and return its handle.

        Memory limits (see docs/design/memory-limits.md for the full design):

        - ``memory_min_mb``: guaranteed floor in MiB. Hard-reserved via
          cgroup v2 ``memory.min``; the apiserver also uses it as the
          scheduler reservation, so requests are rejected with 503
          NoCapacity if no Edge has enough free memory.
        - ``memory_max_mb``: hard ceiling in MiB. Exceeding it triggers
          OOM kill (scope per ``on_oom``).
        - ``on_oom``: ``"kill_process"`` (default; sandbox survives,
          ``ExecResult.terminated_by == "oom"``) or ``"kill_sandbox"``
          (whole sandbox dies, transitions to status=failed with
          ``terminated_by == "oom"``).

        ``memory_min_mb`` and ``memory_max_mb`` can each be ``None``
        independently. The Anthropic infrastructure-noise study
        recommends ``memory_max_mb`` be ~2-3x ``memory_min_mb`` to leave
        burst headroom; setting them equal removes that headroom.

        Note on units: ``_mb`` is the Docker convention (``-m 1024m``)
        meaning MiB (1024*1024 bytes), not 10^6 bytes.

        Distinguishing OOM causes on the result: ``ExecResult.terminated_by``
        is ``"oom"`` for own-max breach (raise the ceiling or shrink the
        workload) vs ``"oom_edge"`` for collateral damage from Edge-wide
        pressure (retry by re-creating the sandbox so the scheduler picks
        again — re-executing on the same sandbox lands on the same Edge
        under the same pressure).
        """
        body = CreateSandboxRequest(
            image=image,
            substrate=Substrate(substrate),
            env=env or {},
            timeout=timeout,
            resources=ResourceSpec(
                memory_min_mb=memory_min_mb,
                memory_max_mb=memory_max_mb,
            ),
            on_oom=OnOom(on_oom),
        )
        r = await self._http.post("/sandboxes", json=body.model_dump(mode="json"))
        r.raise_for_status()
        info = SandboxInfo.model_validate(r.json())
        return Sandbox(info, self)

    async def list_sandboxes(self) -> list[SandboxInfo]:
        r = await self._http.get("/sandboxes")
        r.raise_for_status()
        return [SandboxInfo.model_validate(s) for s in r.json()]

    async def list_edges(self) -> list[EdgeInfo]:
        r = await self._http.get("/edges")
        r.raise_for_status()
        return [EdgeInfo.model_validate(e) for e in r.json()]

    async def capacity(self) -> list[EdgeCapacity]:
        r = await self._http.get("/capacity")
        r.raise_for_status()
        return [EdgeCapacity.model_validate(c) for c in r.json()]

    async def health(self) -> dict:
        r = await self._http.get("/health")
        r.raise_for_status()
        return r.json()

    # ------------------------------------------------------------------
    # Internal: per-sandbox operations called by Sandbox.* methods.
    # ------------------------------------------------------------------

    async def _exec(
        self,
        sandbox_id: str,
        command: str,
        *,
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        user: str | None = None,
        timeout: float | None = None,
    ) -> ExecResult:
        body = ExecRequest(
            command=command, cwd=cwd, env=env or {}, user=user, timeout=timeout
        )
        r = await self._http.post(
            f"/sandboxes/{sandbox_id}/exec",
            json=body.model_dump(mode="json"),
            timeout=timeout + 30 if timeout else None,
        )
        r.raise_for_status()
        return ExecResult.model_validate(r.json())

    async def _read_file(self, sandbox_id: str, path: str) -> bytes:
        r = await self._http.get(
            f"/sandboxes/{sandbox_id}/file", params={"path": path}
        )
        r.raise_for_status()
        return r.content

    async def _write_file(self, sandbox_id: str, path: str, content: bytes) -> None:
        r = await self._http.put(
            f"/sandboxes/{sandbox_id}/file",
            params={"path": path},
            content=content,
            headers={"Content-Type": "application/octet-stream"},
        )
        r.raise_for_status()

    async def _get_trajectory(self, sandbox_id: str) -> list[TrajectoryEntry]:
        r = await self._http.get(f"/sandboxes/{sandbox_id}/trajectory")
        r.raise_for_status()
        return [TrajectoryEntry.model_validate(e) for e in r.json()]

    async def _kill_sandbox(self, sandbox_id: str) -> None:
        r = await self._http.delete(f"/sandboxes/{sandbox_id}")
        r.raise_for_status()
