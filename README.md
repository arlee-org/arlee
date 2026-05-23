# Arlee

**A**gentic **R**L **E**xecution **E**nvironment

> **Status:** Work in progress. This README describes the intended scope and roadmap; most components are not yet implemented.

## What Arlee Is

Arlee is the execution-environment layer of an agent RL system. The trainer updates the policy and the inference engine generates actions; Arlee runs those actions in isolated sandboxes and emits the observations, rewards, and trajectories the trainer learns from.

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

- **Agentic** — Arlee targets multi-turn, environment-interactive workloads (coding agents, computer-use, long-horizon tool use) — i.e. LLMs acting as policies in temporally extended POMDPs. *Not* aimed at single-turn, preference-based post-training (RLHF, DPO, RLAIF), where the model is a static conditional generator over a single-step MDP.
- **RL** — Arlee is purpose-built for agent RL post-training (rollout, evaluation, policy update). *Not* a production sandbox-serving product (e.g. E2B), and *not* a general-purpose browser-automation or code-interpreter tool (e.g. Playwright, Jupyter kernel-as-a-service).
- **Execution Environment** — the *sandbox where agent actions run* — code, shell, browser / computer-use, repo edits, unit tests, simulated SaaS workflows, verifier / reward functions. Not the trainer, not the inference engine, and not the agent framework above (e.g. LangChain, AutoGen).

## Design Inspiration

Arlee draws directly on the interface design of **DeepSeek Elastic Compute (DSec)** described in the DeepSeek V4 report — in particular, the idea of a single SDK abstracting multiple execution substrates (function call / container / microVM / fullVM) behind a unified command-execution, file-transfer, and TTY surface, plus globally ordered per-sandbox trajectory logs that enable replay and preemption-safe resumption. Arlee aims to be a community implementation of that interface shape; it will not attempt DSec-level optimizations on day one.

## Core Capabilities (Target)

- **Unified sandbox API** — one client surface; execution backend (function-call pool, container, microVM, fullVM) is a parameter.
- **Reset / snapshot / fork** — millisecond-scale restoration of environment state for parallel rollouts and episode resets.
- **Trajectory log** — globally ordered, deterministic-replayable record of every command and result per sandbox.
- **Replay & fast-forward** — recover from trainer preemption without re-executing non-idempotent operations.
- **Verifier / reward execution** — first-class place to run graders, unit tests, and reward functions next to the rollout.
- **Scheduler integration** — drop-in workers for K8s / Slurm / Ray, with backpressure-aware queues.

## Engineering Concerns We Care About

| Area | Concerns |
|---|---|
| **Reliability** | Sync vs async RPC, transport (HTTP/2, Thrift), retry & isolation semantics |
| **Scalability** | K8s object / scheduler limits, horizontal scaling of the control plane |
| **Efficiency** | Memory amplification for TB-scale fleets, image size (esp. computer-use images), density |
| **Observability** | Per-sandbox debuggability, latency attribution, failure root-cause attribution, dashboards |
| **Security** | Sandbox isolation, network policy, legal / robots.txt blocklists, reward-hacking mitigations |
| **Eval ergonomics** | Residential-IP routing, screenshot capture, mirror-backed `apt-get`, large-repo `git pull` |

## Roadmap (Sketch)

1. Unified SDK surface and a container-backed reference implementation
2. Trajectory log format + replay / fast-forward
3. Snapshot / fork primitives
4. microVM backend (Firecracker)
5. Trainer-side integration adapters (rollout workers, verifier hooks)
6. Reliability dashboard and per-substrate density benchmarks

