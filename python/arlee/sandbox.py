"""Sandbox handle — the primary user-facing object for per-sandbox operations."""

from __future__ import annotations

from pathlib import Path
from types import TracebackType
from typing import TYPE_CHECKING

from arlee.models import ExecResult, SandboxInfo, TrajectoryEntry

if TYPE_CHECKING:
    from arlee.client import Client


class Sandbox:
    """Handle for a single sandbox.

    Constructed by `Client.create_sandbox` (or `arlee.create_sandbox`); should
    not be instantiated directly. Acts as an async context manager that calls
    `kill()` on exit, so the canonical usage is:

        async with arlee.create_sandbox(image="ubuntu:22.04") as sb:
            await sb.exec("echo hello")
    """

    def __init__(self, info: SandboxInfo, client: Client) -> None:
        self._info = info
        self._client = client
        self._killed = False

    # ----- Properties -----

    @property
    def info(self) -> SandboxInfo:
        return self._info

    @property
    def id(self) -> str:
        return self._info.id

    @property
    def edge_id(self) -> str:
        return self._info.edge_id

    @property
    def image(self) -> str:
        return self._info.image

    # ----- Sandbox operations -----

    async def exec(
        self,
        command: str,
        *,
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        user: str | None = None,
        timeout: float | None = None,
    ) -> ExecResult:
        return await self._client._exec(
            self.id, command, cwd=cwd, env=env, user=user, timeout=timeout
        )

    async def read_file(self, path: str) -> bytes:
        return await self._client._read_file(self.id, path)

    async def write_file(self, path: str, content: bytes) -> None:
        return await self._client._write_file(self.id, path, content)

    async def upload_file(self, source: Path | str, target: str) -> None:
        """Read a local file and place it inside the sandbox at `target`."""
        data = Path(source).read_bytes()
        await self.write_file(target, data)

    async def download_file(self, source: str, target: Path | str) -> None:
        """Copy a file from the sandbox to a local path; creates parent dirs."""
        data = await self.read_file(source)
        out = Path(target)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_bytes(data)

    async def get_trajectory(self) -> list[TrajectoryEntry]:
        return await self._client._get_trajectory(self.id)

    async def kill(self) -> None:
        if self._killed:
            return
        await self._client._kill_sandbox(self.id)
        self._killed = True

    # ----- Async context manager (auto-kill) -----

    async def __aenter__(self) -> Sandbox:
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        await self.kill()
