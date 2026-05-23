"""Arlee Python SDK — async HTTP client against Apiserver."""

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
    SandboxInfo,
    Substrate,
    TrajectoryEntry,
)


class Client:
    def __init__(self, apiserver: str, token: str, timeout: float = 30.0):
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
    # Sandbox lifecycle
    # ------------------------------------------------------------------

    async def create_sandbox(
        self,
        image: str,
        substrate: Substrate | str = Substrate.CONTAINER,
        env: dict[str, str] | None = None,
        timeout: float | None = None,
    ) -> SandboxInfo:
        body = CreateSandboxRequest(
            image=image,
            substrate=Substrate(substrate),
            env=env or {},
            timeout=timeout,
        )
        r = await self._http.post("/sandboxes", json=body.model_dump(mode="json"))
        r.raise_for_status()
        return SandboxInfo.model_validate(r.json())

    async def kill_sandbox(self, sandbox_id: str) -> None:
        r = await self._http.delete(f"/sandboxes/{sandbox_id}")
        r.raise_for_status()

    # ------------------------------------------------------------------
    # Sandbox operations
    # ------------------------------------------------------------------

    async def exec(
        self,
        sandbox_id: str,
        command: str,
        timeout: float | None = None,
    ) -> ExecResult:
        body = ExecRequest(command=command, timeout=timeout)
        r = await self._http.post(
            f"/sandboxes/{sandbox_id}/exec",
            json=body.model_dump(mode="json"),
            timeout=timeout + 10 if timeout else 300.0,
        )
        r.raise_for_status()
        return ExecResult.model_validate(r.json())

    async def read_file(self, sandbox_id: str, path: str) -> bytes:
        r = await self._http.get(
            f"/sandboxes/{sandbox_id}/file",
            params={"path": path},
        )
        r.raise_for_status()
        return r.content

    async def write_file(self, sandbox_id: str, path: str, content: bytes) -> None:
        r = await self._http.put(
            f"/sandboxes/{sandbox_id}/file",
            params={"path": path},
            content=content,
            headers={"Content-Type": "application/octet-stream"},
        )
        r.raise_for_status()

    async def get_trajectory(self, sandbox_id: str) -> list[TrajectoryEntry]:
        r = await self._http.get(f"/sandboxes/{sandbox_id}/trajectory")
        r.raise_for_status()
        return [TrajectoryEntry.model_validate(e) for e in r.json()]

    # ------------------------------------------------------------------
    # Listings
    # ------------------------------------------------------------------

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
