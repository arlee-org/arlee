# Arlee v0 Plan

This document specifies the v0 design and the verifiable checkpoints that define "done".

> **Status: v0 acceptance MET (2026-05-24).** 3/3 SWE-bench Verified gold
> patches resolved across 2 GCP Edge VMs. See "Validation record" at the
> bottom for the run details.

## Acceptance criterion

3 SWE-bench Verified gold patches pass via Arlee, sandboxes distributed across 2 Edge VMs on GCP. No LLM in the loop — gold patches act as the agent, which makes v0 a regression test for Arlee itself.

## Language split

| Component | Language | Why |
|---|---|---|
| Apiserver, Edge, CLI | **Rust** (axum + tokio + bollard, clap) | Long-running services + DSec parity; per-host density / RPC performance ceiling matters as we scale |
| SDK | **Python** (httpx + pydantic) | RL ecosystem (verl / slime / TRL / OpenRLHF) is Python; SDK has to be importable from the consumer's training loop |

Wire protocol between every pair is plain HTTP + JSON (intra-VPC), so the language split is invisible on the wire.

## Architecture

Three runtime components, all in cloud. The only thing on the developer's laptop is a thin CLI (Rust HTTP client wrapping Terraform and Apiserver calls).

```
   Laptop:  arlee CLI ──┐
                        │ HTTPS (token auth, IP-allowlisted)
                        ▼
   ┌─ GCP project, single VPC ─────────────────────────────┐
   │                                                       │
   │   Apiserver VM (e2-small)                             │
   │     └── arlee-apiserver (Rust, axum)                  │
   │           ▲                                           │
   │           │ HTTP (token, intra-VPC)                   │
   │           ▼                                           │
   │   Edge VM #1, #2 (e2-standard-4 × 2)                  │
   │     └── arlee-edge (Rust, axum + bollard) + dockerd   │
   │                                                       │
   └───────────────────────────────────────────────────────┘
```

## Components

### Apiserver (`crates/arlee-apiserver`)

- Rust binary built on axum + tokio + reqwest, on its own e2-small VM.
- In-memory state: Edge registry + sandbox→edge mapping. Not persisted. On restart, queries each Edge that re-registers to rebuild the sandbox map.
- Receives Edge self-registration (`POST /edges/register`) and heartbeat (`POST /edges/{id}/heartbeat`).
- Schedules new sandboxes via least-loaded placement (Edge with fewest active sandboxes).
- Acts as a forwarding proxy: client API calls land here, get routed via `reqwest` to the Edge owning that sandbox.

### Edge (`crates/arlee-edge`)

- One per host. Rust binary built on axum + tokio + bollard, listening on `:8081`.
- On startup, `POST /edges/register` to the Apiserver (URL + token from systemd EnvironmentFile). Re-registers on heartbeat 404.
- Periodic 10-second heartbeat carrying current sandbox count.
- Drives the host's Docker daemon via `bollard`: containers run `sleep infinity` with the image's entrypoint stripped, so we can `docker exec` arbitrary commands.
- Serializes `exec`, `read_file`, `write_file` per sandbox via a per-sandbox `tokio::sync::Mutex`; runs sandboxes concurrently.
- Caps `stdout`/`stderr` per exec at 64 KB with truncation flags so trajectory JSONL stays bounded.
- Writes a JSONL trajectory file per sandbox under `/var/arlee/trajectories/<sandbox-id>.jsonl`, plus a metadata sidecar.

### Python SDK (`python/arlee/`)

- `httpx` async client. Three files: `models.py` (pydantic wire types), `client.py` (Client class + connection pool + raw HTTP calls), `sandbox.py` (Sandbox handle class with per-sandbox methods).
- **Primary API: module-level + Sandbox-as-object.**
  ```python
  async with await arlee.create_sandbox(image=...) as sb:
      await sb.exec("cmd", cwd=..., env=..., user=..., timeout=...)
      await sb.write_file("/p", b"...")        # in-memory bytes
      await sb.upload_file("local.txt", "/remote.txt")  # local-path streaming
      traj = await sb.get_trajectory()
  # sb.kill() on context exit
  ```
- `arlee.Client` is exposed for advanced/multi-cluster use; `create_sandbox` on either Client or the module returns a `Sandbox` handle.
- `arlee.configure(apiserver=..., token=..., timeout=...)` overrides env vars before first SDK call.
- `substrate="container"` is the only valid value in v0; parameter shipped on day 1 for forward compat with microVM/fullVM.
- Consumers: `examples/`, future RL framework adapters (verl / slime / etc).
- Distributed via PyPI as `pip install arlee` (post-v0; v0 is from-source only).

### CLI (`crates/arlee-cli`, binary name `arlee`)

- Rust binary, clap-based.
- `arlee deploy` / `arlee destroy` — shell out to `terraform apply` / `destroy`.
- `arlee edges` / `arlee sandboxes` — query Apiserver, print tables.
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
| `POST /sandboxes/{id}/exec` | `sb.exec` | body: `command`, `cwd?`, `env?`, `user?`, `timeout?`; returns `exit_code`, `stdout`, `stderr` |
| `GET /sandboxes/{id}/file?path=...` | `read_file` | binary-safe; path passed as query param to avoid URL-encoding pain |
| `PUT /sandboxes/{id}/file?path=...` | `write_file` | binary body; same path-as-query convention |
| `GET /sandboxes/{id}/trajectory` | `get_trajectory` | returns JSON array of entries |
| `GET /sandboxes` | `list_sandboxes` | includes status + owning Edge |
| `GET /edges` | `list_edges` | includes health + sandbox count |
| `GET /capacity` | `capacity` | per-Edge sandbox count + health |
| `GET /health` | `health` | open endpoint (no token); reports edge counts |

Edge-internal endpoints (called by Edges, not clients):
- `POST /edges/register` — Edge self-registration on startup
- `POST /edges/{id}/heartbeat` — every 10 seconds; 404 triggers re-registration

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
├── Cargo.toml                          # Rust workspace
├── crates/
│   ├── arlee-models/                   # shared serde wire types
│   ├── arlee-apiserver/                # binary: API gateway + scheduler
│   │   └── src/{main,api,state,scheduler,config,error}.rs
│   ├── arlee-edge/                     # binary: per-host docker driver
│   │   └── src/{main,api,docker_runner,trajectory,config,error}.rs
│   └── arlee-cli/                      # binary: `arlee` command
│       └── src/main.rs
├── python/
│   ├── pyproject.toml                  # hatchling, single `arlee` package
│   └── arlee/
│       ├── __init__.py                 # module-level convenience functions
│       ├── client.py                   # httpx async client
│       └── models.py                   # pydantic models matching arlee-models
├── deploy/
│   ├── terraform/gcp/
│   │   ├── main.tf
│   │   ├── variables.tf
│   │   ├── outputs.tf
│   │   └── cloud-init.yaml.tftpl       # rendered with token + apiserver URL
│   ├── systemd/
│   │   ├── arlee-apiserver.service
│   │   └── arlee-edge.service
│   └── README.md
├── examples/
│   └── swebench_runner.py
├── docs/
│   ├── dsec.md
│   └── v0-plan.md                      # this file
├── README.md
├── LICENSE
└── .gitignore
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
| gRPC / custom binary RPC | HTTP+JSON sufficient at v0 scale; easier to debug |
| Rust SDK (PyO3 bindings) | SDK is Python-only in v0; if a Rust trainer ever wants it, expose via separate Rust client crate later |
| TLS, Secret Manager | Intra-VPC + shared token is acceptable for v0 threat model |
| Persistent Apiserver state | Rebuild from Edges on restart |
| Trainer adapter (verl / slime) | SDK is designed for trainers as first-class consumers, but no adapter ships in v0 |
| Streaming exec output | v0 returns full stdout/stderr after exec completes |

## Validation record

| Checkpoint | Run | Result |
|---|---|---|
| A (hello-world cloud sandbox) | 2026-05-23 ubuntu:22.04 via Apiserver→Edge | ✅ exec / file ops / trajectory all green |
| B (3 SWE-bench gold patches) | 2026-05-23 parallel run | ✅ 3/3 RESOLVED, distributed across 2 Edges (django on edge-1; sympy + sklearn on edge-2) |

Instance IDs used for B:
- `sympy__sympy-14711`
- `django__django-12419`
- `scikit-learn__scikit-learn-14141`

Notable post-v0-acceptance bugs found during validation (already fixed):
- **Scheduler race**: pre-fix least-loaded only read sandbox_count (10s heartbeat lag), so 3 concurrent picks all stacked on the same Edge. Fixed by adding `pick_least_loaded` to State which atomically picks + optimistically increments under a write lock; failure paths call `release_reservation` to roll back.
- **Dev/IaC leak**: our dev GCP project ID (`arlee-497222`) was the default for `var.project_id`. Removed the default; now a required variable. The dev value lives only in the gitignored `terraform.tfvars`.
