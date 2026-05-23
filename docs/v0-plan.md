# Arlee v0 Plan

This document specifies the v0 design and the verifiable checkpoints that define "done".

## Acceptance criterion

3 SWE-bench Verified gold patches pass via Arlee, sandboxes distributed across 2 Edge VMs on GCP. No LLM in the loop — gold patches act as the agent, which makes v0 a regression test for Arlee itself.

## Architecture

Three runtime components, all in cloud. The only thing on the developer's laptop is a thin CLI (HTTP client wrapping Terraform and Apiserver calls).

```
   Laptop:  arlee CLI ──┐
                        │ HTTPS (token auth, IP-allowlisted)
                        ▼
   ┌─ GCP project, single VPC ─────────────────────────────┐
   │                                                       │
   │   Apiserver VM (e2-small)                             │
   │     └── Apiserver process (FastAPI)                   │
   │           ▲                                           │
   │           │ HTTP (token, intra-VPC)                   │
   │           ▼                                           │
   │   Edge VM #1, #2 (e2-standard-4 × 2)                  │
   │     └── Edge process + Docker daemon                  │
   │                                                       │
   └───────────────────────────────────────────────────────┘
```

## Components

### Apiserver

- FastAPI HTTP server, on its own e2-small VM.
- In-memory state: Edge registry + sandbox registry. Not persisted. On restart, queries each known Edge to rebuild state.
- Receives Edge self-registration (`POST /register`).
- Schedules new sandboxes via least-loaded placement (Edge with fewest active sandboxes).
- Acts as a forwarding proxy: client API calls land here, get routed to the Edge owning that sandbox.

### Edge

- One per host. FastAPI HTTP server on `:8081`.
- On startup, `POST /register` to the Apiserver (URL + token from systemd EnvironmentFile).
- Periodic heartbeat with current sandbox count.
- Wraps the host's Docker daemon via the `docker` Python SDK.
- Writes a JSONL trajectory file per sandbox under `/var/arlee/trajectories/<sandbox-id>.jsonl`, plus a metadata sidecar.
- Serializes `exec` calls within a single sandbox (no concurrent commands per sandbox); runs sandboxes concurrently.

### Python SDK (`arlee` package)

- `httpx` async client. Each method maps 1:1 to an Apiserver endpoint.
- `create_sandbox(image=..., substrate="container", env=..., timeout=...)` — `substrate` parameter is exposed on day 1 with `"container"` as the only valid value. This keeps the API forward-compatible when microVM / fullVM substrates land later.
- Consumers: `examples/`, future RL framework adapters (verl / slime / etc).

### CLI (`arlee` command, same package)

- `arlee deploy` / `arlee destroy` — wrap `terraform apply` / `destroy`.
- `arlee edges` / `arlee sandboxes` — list registries.
- `arlee logs <sandbox-id> [--download PATH]` — fetch trajectory.
- `arlee health` — overall component check.

No `submit` / `jobs` / `runs` commands. Job orchestration is the consumer's concern, not Arlee's.

### Terraform module (`deploy/terraform/gcp/`)

Inputs: `project_id`, `region`, `zone`, `edge_count` (default 2).

Resources:
- 1 × e2-small (Apiserver)
- N × e2-standard-4 (Edges)
- VPC + subnet + firewall rules: Apiserver accepts client traffic on its API port; Edges accept only intra-VPC traffic from the Apiserver subnet.
- `cloud-init`: installs Docker, `pip install` arlee (from git), enables systemd units.
- Shared auth token generated via `random_password`, written to each VM's env file.

Outputs: `apiserver_ip`, `edge_ips`, `token` (sensitive).

### SWE-bench gold runner (`examples/swebench_runner.py`)

- Hard-codes 3 SWE-bench Verified task IDs (chosen for small image + short test command).
- Per task: create sandbox with the task's published Docker image → `write_file` the gold patch → `exec` `git apply` → `exec` test command → check exit code → fetch trajectory → kill sandbox.
- All 3 launched concurrently via `asyncio.gather` so the Apiserver places them across both Edges.

## API surface

| Endpoint | SDK method | Notes |
|---|---|---|
| `POST /sandboxes` | `create_sandbox` | body: `image`, `substrate`, `env`, `timeout` |
| `DELETE /sandboxes/{id}` | `kill_sandbox` | trajectory retained 24h after kill |
| `POST /sandboxes/{id}/exec` | `exec` | body: `command`, `timeout`; returns `exit_code`, `stdout`, `stderr` |
| `GET /sandboxes/{id}/files/{path}` | `read_file` | binary-safe |
| `PUT /sandboxes/{id}/files/{path}` | `write_file` | binary-safe |
| `GET /sandboxes/{id}/trajectory` | `get_trajectory` | returns JSONL |
| `GET /sandboxes` | `list_sandboxes` | includes status + owning Edge |
| `GET /edges` | `list_edges` | includes health + sandbox count |
| `GET /capacity` | `capacity` | per-Edge remaining schedulable capacity |

Not in v0: TTY / PTY, snapshot, fork, replay, fast-forward, streaming exec output, Watcher component.

## Trajectory schema

One JSONL file per sandbox, entries strictly ordered by `seq`:

```json
{
  "seq": 0,
  "ts": "2026-05-23T10:00:00.123Z",
  "cmd": "exec",
  "args": {"command": "pytest tests/", "timeout": 60},
  "result": {"exit_code": 0, "stdout_truncated": false, "stdout": "...", "stderr": "..."},
  "result_hash": "sha256:..."
}
```

Plus a metadata sidecar `/var/arlee/trajectories/<id>.meta.json`:

```json
{
  "sandbox_id": "...",
  "created_at": "...",
  "image": "swebench/...",
  "image_digest": "sha256:...",
  "substrate": "container",
  "env": {...},
  "edge_id": "...",
  "killed_at": null
}
```

Together these capture enough to satisfy DSec's three trajectory uses (provenance, fast-forward, deterministic replay) — v0 only implements provenance, but the schema does not block the other two from being added as pure additions later.

## Auth & networking

- Apiserver ↔ Edge: shared token in HTTP header `X-Arlee-Token`. Intra-VPC traffic only, no TLS.
- Client ↔ Apiserver: same token. Apiserver firewall rule allowlists the operator's egress IP.
- Token delivered via systemd `EnvironmentFile`, never written into the image or the repo.

## Repository structure

```
arlee/
├── apiserver/
│   ├── main.py          # FastAPI app
│   ├── state.py         # edges + sandboxes registries
│   └── scheduler.py     # least-loaded placement
├── edge/
│   ├── main.py
│   ├── docker_runner.py
│   └── trajectory.py    # JSONL writer
├── sdk/
│   ├── client.py        # httpx async client
│   └── models.py        # shared pydantic models
├── cli/
│   └── __main__.py
└── pyproject.toml       # uv
deploy/
├── terraform/gcp/
│   ├── main.tf
│   ├── variables.tf
│   ├── outputs.tf
│   └── cloud-init.yaml
├── systemd/
│   ├── arlee-apiserver.service
│   └── arlee-edge.service
└── README.md
examples/
└── swebench_runner.py
docs/
├── dsec.md
└── v0-plan.md           # this file
```

## Verifiable checkpoints

Only one mid-point is meaningful as a gate; everything else is naturally part of moving toward the acceptance criterion.

### Checkpoint A — Hello-world cloud sandbox

```bash
arlee deploy
arlee edges                      # both Edges show as healthy
ssh <apiserver_ip>
python -c "
import asyncio, arlee
async def main():
    sb = await arlee.create_sandbox(image='ubuntu:22.04')
    print(await arlee.exec(sb.id, 'echo hello'))
    print(await arlee.get_trajectory(sb.id))
    await arlee.kill_sandbox(sb.id)
asyncio.run(main())
"
# expect: stdout='hello\n', trajectory has 1 entry
```

Passing A proves the entire infrastructure path works end-to-end: Terraform → cloud-init → Apiserver → Edge registration → scheduling → Docker integration → SDK → trajectory log.

### Checkpoint B — SWE-bench gold patches (v0 acceptance)

```bash
ssh <apiserver_ip>
python examples/swebench_runner.py --gold --n 3
# expect: 3/3 PASS
arlee logs <sandbox-ids> --download ./traj/
# each trajectory contains the patch write, git apply, and test invocation
```

The work between A and B is SWE-bench-specific (task image pull, patch application, test command extraction). Failures there indicate SWE-bench integration issues, not Arlee architecture issues.

## Explicitly deferred (v0.x or later)

| Item | Why deferred |
|---|---|
| TTY / PTY | DSec interface parity, but SWE-bench tasks don't need it |
| microVM (Firecracker) | Needs a separate image workflow |
| fullVM (QEMU) | Lower priority than microVM |
| Function-call pre-warmed pool | Cold-start optimization, irrelevant at v0 scale |
| Snapshot / fork / replay | Trajectory schema already accommodates them; pure-addition later |
| Watcher component | Apiserver's Edge registry is already observable; Watcher can read it |
| EROFS / 3FS layered image loading | Scale optimization mismatched with v0 |
| gRPC transport | HTTP+JSON sufficient at v0 scale; easier to debug |
| TLS, Secret Manager | Intra-VPC + shared token is acceptable for v0 threat model |
| Persistent Apiserver state | Rebuild from Edges on restart |
| Trainer adapter (verl / slime) | SDK is designed for trainers as first-class consumers, but no adapter ships in v0 |
| Streaming exec output | v0 returns full stdout/stderr after exec completes |
