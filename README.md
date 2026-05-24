# Arlee

Arlee (**A**gentic **RL** **E**xecution **E**nvironment) is the execution-environment layer of an agent RL system. The trainer updates the policy and the inference engine generates actions; Arlee runs those actions in isolated sandboxes and emits the observations, rewards, and trajectories the trainer learns from.

```
                    RL trainer  (GPU cluster)
                       │              ▲
                policy │              │ trajectories
                       ▼              │
   ┌─ many parallel episodes ─────────────────────────┐
   │                                                  │
   │     rollout / inference  ──action──▶  Arlee      │
   │             ▲                          │         │
   │             └──────── observation ─────┘         │
   │           (repeats for many turns per episode)   │
   │                                                  │
   └──────────────────────────────────────────────────┘
```

## Scope

The three words in the name each carve out part of the scope:

- **Agentic** — Arlee targets multi-turn, environment-interactive workloads: coding agents, computer-use, long-horizon tool use — the model takes an action, sees what happens, and decides the next action, over many turns. *Not* aimed at single-turn, preference-based post-training (RLHF, DPO, RLAIF), where the model generates one response with no environment to act in.
- **RL** — Arlee is purpose-built for agent RL post-training (rollout, evaluation, policy update). *Not* a production sandbox-serving product (e.g. E2B), and *not* a general-purpose browser-automation or code-interpreter tool (e.g. Playwright, Jupyter kernel-as-a-service).
- **Execution Environment** — the *sandbox where agent actions run* — code, shell, browser / computer-use, repo edits, unit tests, simulated SaaS workflows, verifier / reward functions. Not the trainer, not the inference engine, and not the agent framework above (e.g. LangChain, AutoGen).

## Design Inspiration

Arlee draws directly on the interface design of **DeepSeek Elastic Compute (DSec)** described in the [DeepSeek V4 report](https://huggingface.co/deepseek-ai/DeepSeek-V4-Pro/blob/main/DeepSeek_V4.pdf) — in particular, the idea of a single SDK abstracting multiple execution substrates (function call / container / microVM / fullVM) behind a unified command-execution, file-transfer, and TTY surface, plus globally ordered per-sandbox trajectory logs that enable replay and preemption-safe resumption. Arlee aims to be a community implementation of that interface shape; it will not attempt DSec-level optimizations on day one.

## What's built

### Architecture

| Component | Language | Runs where | Job |
|---|---|---|---|
| `arlee-apiserver` | Rust (axum) | One cloud VM | HTTP API gateway, edge registry, memory-aware spread scheduler with optimistic reservation, forwarding proxy to Edges |
| `arlee-edge` | Rust (axum + bollard) | Each cloud VM with Docker | Per-host sandbox driver; runs containers, serializes exec per sandbox, writes JSONL trajectory |
| `arlee` Python SDK | Python (httpx + pydantic) | Inside the consumer (eval script, RL trainer adapter, …) | Async client over the HTTP API |
| `arlee` CLI | Rust (clap) | Operator's laptop | Thin wrapper for `terraform`, edge/sandbox/log queries |
| Terraform module | HCL | Operator's laptop → GCP | Provisions Apiserver VM + N Edge VMs + VPC + firewalls |
| GitHub Actions | YAML | CI | Builds `arlee-apiserver` and `arlee-edge` on push to main, publishes to a rolling `main-latest` release the VMs `curl` on first boot |

Auth between every component is a single shared token in the `X-Arlee-Token` header, intra-VPC only, no TLS.

### Interfaces

**Python SDK** — what RL trainers / eval harnesses call into. Module-level
entry + sandbox-as-object + async-context auto-cleanup:

```python
import arlee
# ARLEE_APISERVER + ARLEE_TOKEN from env, or arlee.configure(apiserver=..., token=...)

async with await arlee.create_sandbox(
    image="ubuntu:22.04",
    memory_min_mb=1024,         # guaranteed floor (cgroup memory.min)
    memory_max_mb=3072,         # hard ceiling (cgroup memory.max)
    on_oom="kill_process",      # default; or "kill_sandbox"
) as sb:
    res = await sb.exec("pytest tests/", cwd="/testbed", env={"FOO": "bar"})
    if res.terminated_by == "oom":
        print("workload exceeded its own ceiling; raise memory_max_mb")
    elif res.terminated_by == "oom_edge":
        print("Edge-pressure OOM; re-create the sandbox to retry on a fresh placement")
    await sb.write_file("/tmp/patch.diff", patch_bytes)
    contents = await sb.read_file("/tmp/output")
    trajectory = await sb.get_trajectory()
# sb.kill() runs on context exit
```

The apiserver schedules each sandbox onto the Edge with the most available memory headroom (spread, not pack), reserving `memory_min_mb` on that Edge as a hard cgroup-enforced floor. Memory fields are optional; omit both to inherit the current host-default (no limits, no reservation). See [docs/memory-limits.md](docs/memory-limits.md) for the full operational reference (and [docs/archive/memory-limits.md](docs/archive/memory-limits.md) for the original design + validation record).

For multi-cluster / explicit lifecycle control, `arlee.Client(apiserver=..., token=...)` is still exposed; `client.create_sandbox(...)` returns the same `Sandbox` handle.

**HTTP API** — language-agnostic surface the SDK targets:

| | |
|---|---|
| `POST /sandboxes` | create |
| `DELETE /sandboxes/{id}` | kill |
| `POST /sandboxes/{id}/exec` | run a command (`cwd`, `env`, `user`, `timeout` all optional) |
| `GET / PUT /sandboxes/{id}/file?path=...` | binary-safe file read/write |
| `GET /sandboxes/{id}/trajectory` | JSONL trajectory |
| `GET /sandboxes` / `GET /edges` / `GET /capacity` | introspection |

**CLI** — operator tooling: `arlee deploy / destroy / health / edges / sandboxes / logs`.

**Terraform** — `deploy/terraform/gcp/` provisions everything; `var.project_id` is the only thing without a safe default.

### Quick start

See [deploy/README.md](deploy/README.md) for the full 5-minute walkthrough. TL;DR:

```bash
cd deploy/terraform/gcp
cp terraform.tfvars.example terraform.tfvars   # fill in project_id + operator_ip_cidr
terraform init && terraform apply
eval "$(terraform output -raw env_setup)"      # exports ARLEE_APISERVER + ARLEE_TOKEN
arlee health                                    # both edges should be healthy
```

To run the SWE-bench demo (3 gold patches, no LLM — serves as an infra regression test):

```bash
gcloud compute ssh arlee-apiserver --zone=us-central1-a --tunnel-through-iap --project=$PROJECT_ID
sudo /opt/arlee-venv/bin/python /opt/arlee/examples/swebench_runner.py --gold
# expect: 3/3 RESOLVED
```

## Core Capabilities (Target)

- **Unified sandbox API** — one client surface; execution backend (function-call pool, container, microVM, fullVM) is a parameter.
- **Reset / snapshot / fork** — millisecond-scale restoration of environment state for parallel rollouts and episode resets.
- **Trajectory log** — globally ordered, deterministic-replayable record of every command and result per sandbox.
- **Replay & fast-forward** — recover from trainer preemption without re-executing non-idempotent operations.
- **Verifier / reward execution** — first-class place to run graders, unit tests, and reward functions next to the rollout.
- **Scheduler integration** — drop-in workers for K8s / Slurm / Ray, with backpressure-aware queues.

## Design considerations

Ongoing areas we're paying attention to as the system grows beyond what's currently shipped:

| Area | What we're tracking |
|---|---|
| **Reliability** | Sync vs async RPC, transport (HTTP/2, Thrift), retry & isolation semantics |
| **Scalability** | K8s object / scheduler limits, horizontal scaling of the control plane |
| **Efficiency** | Memory amplification for TB-scale fleets, image size (esp. computer-use images), density |
| **Observability** | Per-sandbox debuggability, latency attribution, failure root-cause attribution, dashboards |
| **Security** | Sandbox isolation, network policy, legal / robots.txt blocklists, reward-hacking mitigations |
| **Eval ergonomics** | Residential-IP routing, screenshot capture, mirror-backed `apt-get`, large-repo `git pull` |

## Roadmap (Sketch)

1. ✅ Unified SDK surface and a container-backed reference implementation
2. Trajectory log format + replay / fast-forward (schema in place; replay not yet implemented)
3. Snapshot / fork primitives
4. microVM backend (Firecracker)
5. Trainer-side integration adapters (rollout workers, verifier hooks)
6. Reliability dashboard and per-substrate density benchmarks

