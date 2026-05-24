# Per-Sandbox Memory Limits

Operational reference for the memory-limits feature: API shape, semantics,
implementation details, testing. Pointers in code and other docs link to
specific sections here.

> Looking for the original design discussion (reference-point survey,
> deliberate-non-abstraction rationale, rollout plan, validation record)?
> See [archive/memory-limits.md](archive/memory-limits.md) — that's a
> frozen snapshot from when the feature first shipped.

## 1. Goals & non-goals

**Goals**
1. Two user-facing knobs per sandbox: a guaranteed floor (`memory_min_mb`) and a hard ceiling (`memory_max_mb`).
2. Floor is a real reservation — kernel-enforced, not merely a scheduler hint.
3. Ceiling triggers OOM kill, with the option of either killing only the offending process or the whole sandbox.
4. Failure causes (OOM vs timeout vs container death) are surfaced explicitly in API responses.
5. Wire protocol generalizes across the four substrates documented in [dsec.md](dsec.md), even though only `container` is implemented now.

**Non-goals** (each could be added if a real need shows up; none are currently asked for)
1. Soft caps with throttling (cgroup `memory.high`).
2. Swap tuning beyond "off" — swap on agent workloads silently masks bugs.
3. CPU, disk, GPU, network limits. Same shape, separate design.
4. Preemption / eviction. If a new sandbox doesn't fit, the API returns `NoCapacity` and the caller retries or scales out.
5. Autoscaling Edges. The spread scheduler (§3.2) assumes manual scaling.

## 2. User-facing API

### 2.1 Python SDK

```python
import arlee

async with arlee.create_sandbox(
    image="ubuntu:22.04",
    memory_min_mb=1024,           # guaranteed floor; kernel-enforced
    memory_max_mb=3072,           # hard ceiling; OOM on exceed
    on_oom="kill_process",        # default; alternative: "kill_sandbox"
) as sb:
    result = await sb.exec("python train.py")
    if result.terminated_by == "oom":
        print(f"OOM-killed at exit {result.exit_code}")
        # sandbox is still alive — can keep going
```

All three new fields are optional. Defaults: `memory_min_mb=None`, `memory_max_mb=None`, `on_oom="kill_process"`. Passing both memory fields as `None` yields no-limits behavior — the same as before memory limits existed, so SWE-bench-style callers that don't set them see no change.

### 2.2 Wire schema (REST)

`POST /sandboxes` body, with a `resources` sub-object and a top-level `on_oom`:

```json
{
  "image": "ubuntu:22.04",
  "resources": {
    "memory_min_mb": 1024,
    "memory_max_mb": 3072
  },
  "on_oom": "kill_process"
}
```

The `resources` namespace is reserved for future CPU / disk / GPU fields. `on_oom` is at the top level because it concerns sandbox lifecycle, not a single resource.

### 2.3 Response enrichments

Two parallel "why did X end" enums exist, at exec scope and sandbox scope. Names are deliberately symmetric: same field name `terminated_by` on both, same type-name suffix `Termination`.

`ExecResult` has `terminated_by` (exec scope — "what ended this exec command"):

```rust
pub struct ExecResult {
    // ... existing fields
    pub terminated_by: Option<ExecTermination>, // None = exited on its own
}

pub enum ExecTermination {
    Oom,            // exec exceeded this sandbox's own memory_max_mb
    OomEdge,        // killed by system OOM killer due to Edge-wide memory pressure;
                    // this sandbox may have been well under its own max
    Timeout,        // killed by Arlee's exec timeout
    ContainerDied,  // container died mid-exec (non-OOM)
}
```

`SandboxInfo` echoes the configured values and has `terminated_by` at sandbox scope ("what ended this sandbox"):

```rust
pub struct SandboxInfo {
    // ... existing fields
    pub resources: ResourceSpec,
    pub on_oom: OnOom,
    pub terminated_by: Option<SandboxTermination>, // None while Running
}

pub enum SandboxTermination {
    UserKilled,        // kill() called
    Oom,               // container died from its own memory.max breach
    OomEdge,           // container died from Edge-wide pressure (rare; PID 1 has
                       // oom_score_adj=-1000 under kill_process, immune from global
                       // OOM; can still happen under kill_sandbox)
    ContainerCrashed,  // non-OOM container death
}
```

The two enums share `Oom` and `OomEdge` variants intentionally — same root cause, surfaced at the appropriate scope. The exec-scope enum adds `Timeout` and `ContainerDied`; the sandbox-scope enum adds `UserKilled`. Two distinct enums (rather than one superset) so that consumers don't need to defend against variants that can't occur in their context.

`Oom` and `OomEdge` are deliberately distinct types, not a sub-field on a single OOM variant. They have different remediation:

- `Oom` — the workload exceeded the declared ceiling. Not retriable as-is; user should raise `memory_max_mb` or reduce the workload's memory use.
- `OomEdge` — the Edge was over-committed and this sandbox was selected by the system OOM killer as collateral. Retriable by **re-creating the sandbox** (so the scheduler picks again, possibly landing on a less-loaded Edge), not by re-executing on the same sandbox (which is on the same Edge under the same pressure).

We deliberately do not expose a derived `is_retriable` boolean — that would encourage the wrong retry mode (re-exec). Consumers read `terminated_by` and decide.

When `terminated_by` is set, stderr is appended with a clear marker line, matching the existing timeout convention:

```
arlee: process was OOM-killed (sandbox exceeded memory_max_mb=3072MiB)
arlee: process was OOM-killed (Edge memory pressure; this sandbox was at 5120MiB of 8192MiB max)
arlee: exec timed out after 30s
```

## 3. Semantics

### 3.1 Memory fields

| Field | Meaning | Implementation |
|---|---|---|
| `memory_min_mb` | Reservation. Apiserver guarantees `sum(mins on Edge) ≤ Edge_total`. Kernel guarantees the bytes are never reclaimed from this sandbox. | cgroup v2 `memory.min` on parent cgroup. |
| `memory_max_mb` | Hard ceiling. Exceed → OOM kill (scope per `on_oom`). | cgroup v2 `memory.max` on parent cgroup; `memory.swap.max=0` to disable swap. |
| Both `None` | No limits; scheduler treats as 0 reservation. Backward-compatible with current behavior. | No `HostConfig.memory*` set. |
| Only `max` | Hard ceiling set; scheduler reserves nothing. Recommended for "I want a safety net but don't need a guaranteed floor." | `memory.max` only. |
| Only `min` | Reservation set; no ceiling. Rare; allowed for completeness. | `memory.min` only. |
| `min > max` | Rejected at API entry with 400. | Validation. |

Units are MiB (1024×1024 bytes) despite the `_mb` suffix — this is the Docker convention (`docker run --memory 1024m` is MiB) shared by E2B, verl, and Harbor.

Caveats:
- The `memory_min_mb` guarantee is kernel-strong while the sandbox runs. It does **not** survive Edge process restart (the apiserver rebuilds Edge state from `/sandboxes` on restart — see [CLAUDE.md](../CLAUDE.md) "Known gotchas"; same window applies).
- Setting `min == max` removes burst headroom. Anthropic's [Infrastructure Noise in Agentic Coding Evals](https://www.anthropic.com/engineering/infrastructure-noise) study found this materially hurts eval pass rate; they recommend `max ≈ 2–3× min`.

### 3.2 Scheduling: spread by available-memory ratio

```
for each healthy Edge e:
  available_after_e = total_e - sum_mins_e - new_sandbox_min
  if available_after_e < 0: skip (infeasible)
  score_e = (total_e - sum_mins_e) / total_e   # higher = emptier
choose e with max score_e; tiebreak by min sandbox_count
```

This is **spread**, not pack. The reasoning:

- Burst headroom (`total - sum_mins`) is shared among all sandboxes on an Edge. Packing tight on `sum_mins` collapses that headroom, making `memory_max_mb > memory_min_mb` theater. Spread preserves it.
- Failure cost is asymmetric: an OOM loses an entire rollout/eval task (minutes of work); an idle Edge costs a few dollars an hour.
- Pack's main benefit (consolidation enabling node deprovisioning) requires an autoscaler, which we do not have.
- Spread reduces blast radius if an Edge dies.

The ratio formulation (vs raw available bytes) is for future heterogeneous Edge sizes; on a homogeneous fleet the two are equivalent.

**No admission control on sum of maxes.** We deliberately allow `sum(maxes) > total` — this is over-commit at the ceiling layer, exactly the Anthropic-recommended `limit = 3× request` shape. Sandboxes compete for the shared headroom; the kernel arbitrates via OOM at `memory.max`.

**The cost of over-commit: Edge OOM.** If multiple sandboxes burst concurrently, the Edge's total memory can be exhausted before any single sandbox breaches its own `memory.max`. The system OOM killer then selects a victim across all cgroups based on `oom_score`. The chosen sandbox may have been well under its own ceiling. This event is reported to the caller as `terminated_by=OomEdge` (vs `Oom` for own-max breach), with the discriminator described in §3.4. The remediation is retry-by-re-creating-the-sandbox, not retry-the-exec — see §2.3.

We accept this cost because:
- Forbidding over-commit collapses burst headroom; `memory_max_mb > memory_min_mb` becomes theater.
- The blast radius is bounded: at most one sandbox dies per OOM event (with default `on_oom=kill_process`); kernel reclaim happens first and often suffices without killing anyone.
- The signal is clean: `OomEdge` is distinct from `Oom` so consumers can react correctly.

If operational data later shows `OomEdge` frequency is unacceptable on real workloads, two remediations are available without changing the API: (a) introduce an apiserver-side `max_overcommit_ratio` config to cap `sum(maxes) / total` at admission, or (b) have the Edge proactively shed sandboxes via PSI (Pressure Stall Information) before kernel OOM fires. Neither is built — we ship honest reporting first, observe, then add control surfaces if warranted.

**Atomic pick-and-reserve.** `pick_with_memory` increments `reserved_memory_mb` (and `sandbox_count`) under the write lock; failure paths call `release_reservation` to roll back. Heartbeats reconcile via `max(apiserver_value, edge_reported)` (drift catch only; never under-count, so a heartbeat arriving between pick and Edge-side create doesn't clobber the optimistic increment). `forget_sandbox` is the only authoritative decrement; it subtracts the killed sandbox's `memory_min_mb` from the Edge's running total.

### 3.3 OOM scope: `on_oom`

| Policy | cgroup setting | Behavior |
|---|---|---|
| `kill_process` (default) | `memory.oom.group=0` | Kernel kills individual processes. Sandbox PID 1 (`sleep infinity`) survives. Sandbox stays `Running`; exec returns `terminated_by=Oom`. |
| `kill_sandbox` | `memory.oom.group=1` | Kernel kills the whole cgroup atomically. Sandbox transitions to `Failed` with `terminated_by=Oom`. All subsequent ops return 502/410. |

Default rationale: lenient is the safer default. With `kill_process`, the caller gets a structured failure signal (`terminated_by=oom`) and can decide whether to retry the command, abandon the sandbox, or raise the ceiling — the sandbox is still around to do any of those. `kill_sandbox` is the explicit opt-in for "any OOM means this sandbox is unrecoverable"; consumers that want a hard error boundary at every OOM rather than an exec-level signal pick this.

Two caveats:

1. `kill_process` does not guarantee that *only* one process is killed. The kernel may kill multiple to satisfy an allocation. Documented as "does not force atomic group-kill," not "kills exactly one process."
2. The kernel could theoretically pick PID 1 as the victim, ending the sandbox even under `kill_process`. We push back against this by writing `oom_score_adj=-1000` on PID 1 at sandbox creation **only when `on_oom=kill_process`** (Docker's default is less aggressive). With `sleep infinity` as PID 1, the residual probability is negligible. Under `kill_sandbox` we deliberately leave PID 1's oom_score_adj at the default — `oom_score_adj=-1000` makes the kernel skip the process even when `memory.oom.group=1` says "kill the whole cgroup", which would defeat the `kill_sandbox` semantic by keeping PID 1 alive after the rest of the cgroup is SIGKILLed.

### 3.4 Detection and reporting

cgroup v2 `memory.events` cleanly distinguishes own-max from Edge-pressure OOMs without any out-of-band signal (no `dmesg`, no global state). The relevant counters per cgroup:

| Counter | When it increments |
|---|---|
| `max` | This cgroup's usage hit `memory.max`. |
| `oom` | This cgroup tried to allocate, reclaim within the cgroup failed → cgroup-scope OOM triggered. |
| `oom_kill` | A process **in this cgroup** was killed by **any** OOM killer (cgroup-scope or system-wide). |

The discriminator:

| Scenario | `oom_kill` delta | `max` / `oom` delta |
|---|---|---|
| Sandbox exceeded its own `memory.max` | > 0 | > 0 |
| Sandbox was selected by system OOM killer (Edge pressure) | > 0 | **0** (never triggered by this cgroup) |

**Exec-level detection**: before and after each exec, read `memory.events` (a single file, one read) and capture all three counters. After the exec:

```
if oom_kill_after > oom_kill_before:
    if max_after > max_before or oom_after > oom_before:
        terminated_by = Oom
    else:
        terminated_by = OomEdge
```

Cost: two file reads per exec, negligible.

**Sandbox-level detection** (currently only via the post-exec path; proactive detection in `require()` is future work — see end of this section): when an exec discovers OOM and `on_oom=kill_sandbox`, the sandbox transitions to `Failed` with `terminated_by` set per the same discriminator. The expected lifecycle on kill is: stop container → inspect for OOM → read cgroup events → rmdir. Cgroup cleanup order matters because once we `rmdir`, the counters are gone.

**PID 1 immunity** (only under `on_oom=kill_process`): we write `oom_score_adj = -1000` on PID 1 at sandbox creation (§4.2), which makes the global OOM killer treat it as immune. So in the `OomEdge` scenario, the kernel picks some other process in the sandbox before considering PID 1 — the sandbox usually survives (returning `OomEdge` on the exec) rather than transitioning to `Failed`. `terminated_by = OomEdge` at the sandbox level is reserved for the rare case where every non-PID-1 process is gone or PID 1 itself was somehow killed. Under `on_oom=kill_sandbox` we deliberately do **not** write `-1000` (see §3.3 caveat 2), so under `kill_sandbox` an Edge-pressure OOM that picks PID 1 will also end the sandbox — the harsher semantic the user opted into.

**stderr enrichment**: described in §2.3 with both flavors.

**Future**: subscribe to cgroup `memory.events` via epoll for push-based state transitions (instead of polling at the next operation). Would let consumers know about OomEdge / sandbox death without making another call. Current consumers (verl, Terminal-Bench) poll terminal state so this hasn't been a blocker. The same async stream would enable proactive sandbox-level detection in `require()`.

### 3.5 Substrate generality

Conceptually the same fields apply to all four substrates from [dsec.md](dsec.md), but with different fidelity. The wire protocol is one shape; per-substrate capabilities are declared and enforced at the apiserver:

| Concept | Container | microVM (Firecracker) | fullVM (QEMU) | Function Call |
|---|---|---|---|---|
| `memory_min_mb` | cgroup `memory.min` | VM boot RAM size (no min/max distinction) | Same as microVM | Pool template setting |
| `memory_max_mb` | cgroup `memory.max` | VM boot RAM size | Same | Pool template setting |
| `min != max` | Supported (elastic) | Rejected — set them equal (balloon driver: future work) | Rejected | Rejected (per-pool, not per-call) |
| `on_oom=kill_process` | `memory.oom.group=0` | Free — guest kernel handles | Same | Not applicable |
| `on_oom=kill_sandbox` | `memory.oom.group=1` | Detect via guest agent → destroy VM | Same | Default and only behavior |
| OOM detection fidelity | High (kernel counters) | Medium (agent-mediated) | Medium | High (call exit) |
| `OomEdge` concept applies | Yes — shared kernel, real noisy-neighbor risk | No — each VM has its own RAM allocation, no shared headroom to compete for | No — same as microVM | No — each call is independent |

Substrate capabilities are declared in code:

```rust
pub struct SubstrateCapabilities {
    pub supports_elastic_memory: bool,           // min != max ok?
    pub supports_on_oom: HashSet<OnOom>,
    pub supports_per_sandbox_memory: bool,       // false for Function Call
}
```

The apiserver validates requests against the chosen substrate's capabilities and **hard-rejects** impossible combinations (e.g., `min != max` with microVM, `on_oom=kill_process` with Function Call) — never silently drops a constraint.

Only `Substrate::Container` is implemented today. The capability struct exists so the wire protocol is stable for the other three when they land — adding a substrate is a new `impl SubstrateRuntime` (§4.1) plus a capability declaration, no protocol churn.

## 4. Implementation

### 4.1 Substrate runtime trait

The wire protocol (§3.5) treats substrate as a first-class concept: `CreateSandboxRequest.substrate`, per-substrate `SubstrateCapabilities`, apiserver-side validation that the requested combination is honored. The Rust layer mirrors this with a trait:

```rust
#[async_trait]
pub trait SubstrateRuntime: Send + Sync {
    fn capabilities(&self) -> &SubstrateCapabilities;
    fn total_memory_mb(&self) -> u32;

    async fn create(&self, req: &CreateSandboxRequest) -> Result<SandboxInfo>;
    async fn kill(&self, sandbox_id: &str) -> Result<()>;
    async fn exec(&self, sandbox_id: &str, req: &ExecRequest) -> Result<ExecResult>;
    async fn read_file(&self, sandbox_id: &str, path: &str) -> Result<Vec<u8>>;
    async fn write_file(&self, sandbox_id: &str, path: &str, content: Vec<u8>) -> Result<()>;
    async fn get_trajectory(&self, sandbox_id: &str) -> Result<Vec<serde_json::Value>>;
    async fn list_infos(&self) -> Vec<SandboxInfo>;
    async fn sandbox_count(&self) -> u32;
    async fn reserved_memory_mb(&self) -> u32;
}
```

The Container substrate is `DockerSubstrate: impl SubstrateRuntime`. The Edge holds `Arc<dyn SubstrateRuntime>`. Dispatch via `Box<dyn>` rather than enum is chosen because (a) only one substrate is active per Edge process, (b) the trait surface is fixed (no hot-path concern that would warrant monomorphization), (c) it allows out-of-tree implementations should they ever be useful (e.g., a test-only `MockSubstrate`).

microVM / fullVM / Function Call substrates will land as additional `impl SubstrateRuntime` blocks plus their respective `SubstrateCapabilities` declarations, without further trait churn.

### 4.2 Edge: cgroup_parent for hard reservation

Docker only exposes `--memory-reservation` (soft, `memory.low`). To get hard `memory.min` we manage a parent cgroup ourselves.

```
per sandbox sid:
  mkdir /sys/fs/cgroup/arlee/<sid>/
  write memory.min        = <memory_min_mb> * MiB
  write memory.max        = <memory_max_mb> * MiB
  write memory.swap.max   = 0
  write memory.oom.group  = 1 if on_oom == kill_sandbox else 0
  docker create --cgroup-parent=/arlee/<sid> ...
  # after container starts, if on_oom == kill_process:
  write /proc/<pid_1>/oom_score_adj = -1000
  # on sandbox kill:
  remove container (docker removes its scope under /arlee/<sid>/)
  rmdir /sys/fs/cgroup/arlee/<sid>/
```

cgroup v2 detail: setting `memory.min` on the parent protects the *total subtree usage*. Because we keep one container per parent cgroup (one sandbox per parent), the protection effectively covers the container's usage. No need to write `memory.min` on the Docker-created child scope.

Edge VM requirements (enforced at Edge startup, fail-fast):
- cgroup v2 mounted at `/sys/fs/cgroup` (`mount | grep cgroup2`).
- Docker configured with `native.cgroupdriver=cgroupfs` (in `/etc/docker/daemon.json`, set by cloud-init **before** `apt-get install docker.io` — otherwise dockerd's postinst auto-start picks up the systemd default). The Docker default on modern systemd hosts is `systemd`; we override.

Stale cgroup reconciliation on Edge startup: list directories under `/sys/fs/cgroup/arlee/`; rmdir any not corresponding to a known sandbox (catches leftover state from a crashed Edge process).

The `EdgeCgroup` module encapsulates these file operations. Pure file IO; unit tests inject the cgroup root path as a tmp dir.

### 4.3 Edge: bollard `HostConfig` changes

```rust
let host_config = bollard::models::HostConfig {
    cgroup_parent: Some(format!("/arlee/{sandbox_id}")),
    // Belt-and-suspenders: pass memory limits to Docker too,
    // so `docker inspect` reflects them. The cgroup_parent values are authoritative.
    memory: req.resources.memory_max_mb.map(|mb| (mb as i64) << 20),
    memory_swap: req.resources.memory_max_mb.map(|mb| (mb as i64) << 20),
    oom_kill_disable: Some(false),
    ..Default::default()
};
```

After `start_container`, read the container's PID via `inspect_container().state.pid`; if `on_oom=kill_process`, write `-1000` to `/proc/<pid>/oom_score_adj`.

### 4.4 Apiserver: scheduler

`EdgeRecord` ([crates/arlee-apiserver/src/state.rs](../crates/arlee-apiserver/src/state.rs)) carries:

```rust
pub struct EdgeRecord {
    // ... existing
    pub total_memory_mb: u32,
    pub reserved_memory_mb: u32,  // sum of mins across this Edge's sandboxes
}
```

`pick_with_memory(min_mb: u32) -> PickResult` implements §3.2. Atomic pick-and-reserve increments both `sandbox_count` and `reserved_memory_mb` under the write lock. `release_reservation` decrements both on failure. `forget_sandbox` is the authoritative decrement on kill (subtracts the sandbox's `memory_min_mb` tracked in a per-sandbox map).

Edge registration and heartbeat carry the new numbers:

```rust
pub struct RegisterEdgeRequest {
    pub edge_id: String,
    pub url: String,
    pub sandbox_count: u32,
    pub total_memory_mb: u32,
    pub reserved_memory_mb: u32,
}

pub struct HeartbeatRequest {
    pub sandbox_count: u32,
    pub reserved_memory_mb: u32,
}
```

Edge derives `total_memory_mb` at startup from `/proc/meminfo` `MemTotal` minus a configurable system reserve (default 512 MiB).

### 4.5 Apiserver: capability validation

A `Substrate -> SubstrateCapabilities` lookup. In `create_sandbox`, after parsing the request:

1. Look up capabilities for `req.substrate`.
2. If `req.resources.memory_min_mb != req.resources.memory_max_mb` and `!caps.supports_elastic_memory`: 400.
3. If `req.on_oom` not in `caps.supports_on_oom`: 400.
4. If `min > max`: 400.

Container's capabilities:
```rust
SubstrateCapabilities {
    supports_elastic_memory: true,
    supports_on_oom: hashset![KillProcess, KillSandbox],
    supports_per_sandbox_memory: true,
}
```

### 4.6 Detection plumbing

Per exec, in `docker_substrate::exec` (per the §3.4 discriminator):

```rust
let (max_before, oom_before, kill_before) = cgroup.read_memory_events(sandbox_id)?;
let exec_result = run_exec(...).await?;
let (max_after, oom_after, kill_after) = cgroup.read_memory_events(sandbox_id)?;

let terminated_by = if kill_after > kill_before {
    if max_after > max_before || oom_after > oom_before {
        Some(ExecTermination::Oom)
    } else {
        Some(ExecTermination::OomEdge)
    }
} else if exec_timed_out {
    Some(ExecTermination::Timeout)
} else {
    None
};
```

Appends the appropriate stderr marker (§2.3) when `terminated_by` is set. When `on_oom=kill_sandbox` and an OOM is detected, also transitions `SandboxInfo.status = Failed` with the corresponding `SandboxTermination`.

## 5. Testing strategy

In order of risk, lowest first:

1. **Validation unit tests** (apiserver): `min > max` → 400. `on_oom` invalid for substrate → 400. `min` exceeds any Edge's `total_memory_mb` → 503 NoCapacity with a clear message.
2. **Cgroup module unit tests** (Edge): inject tmp dir as cgroup root; assert `EdgeCgroup::create(sid, spec)` produces the expected files with the expected contents.
3. **Scheduler unit tests** (apiserver state): build a fixture with 2 Edges (16 GiB each), various reservation levels, assert picks match the spread policy. Specifically test (a) feasibility filtering (rejects infeasible), (b) score ranking (picks emptier), (c) tiebreak (lowest count when scores tie), (d) atomic reservation rollback on failure path.
4. **N=1 integration on a real Edge** (per CLAUDE.md "Debug N=1 first"): create one sandbox with `memory_max_mb=1024`, run `stress --vm-bytes 2G --vm-keep` via exec, assert `terminated_by=Oom` and exit_code non-zero. Then exec again, assert sandbox still alive (`kill_process` default). Repeat with `on_oom=kill_sandbox`: same workload, assert sandbox transitions to `Failed` and subsequent exec errors out.
5. **Hard-reservation test**: 16 GiB Edge, sandbox A with `min=4G max=12G` and sandbox B with `min=2G max=6G`. Make A consume near its max. Then B allocates within its own `memory_min_mb`. Soft reservation: B's allocation may OOM or be slow. Hard reservation: B succeeds immediately. Asserts that `memory.min` is doing real work.
6. **2-Edge scheduling distribution**: 4 sandboxes with `min=2G max=4G` against 2 Edges. Assert distribution is 2-2, not 4-0.
7. **Edge OOM (`OomEdge`) discrimination**: three sandboxes each with `min=2G max=12G`. Concurrent `stress --vm-bytes 9G --vm-keep` in each. Sum on a single Edge (~18 GiB) exceeds Edge capacity but no single sandbox exceeds its own `max=12G`. Assert: at least one exec returns `terminated_by=OomEdge` (judgment test for the whole §3.4 discriminator).
8. **No-regression on the gold demo**: existing SWE-bench `examples/swebench_runner.py --gold` with no memory fields set continues to produce 3/3 RESOLVED. Validates that the `None / None` default path is unchanged.
9. **Anthropic-style noise test** (acceptance, not CI): run a memory-sensitive SWE-bench Verified subset twice — once with `min == max == X`, once with `min = X, max = 3X` — and record pass rate delta. Reproducing Anthropic's qualitative finding (lenient ceiling improves pass rate) validates that the enforcement semantics are correct end-to-end. Not reproducing it is informative too.

All tests are codified, not ad-hoc:

- **1–3 and 8**: Rust unit tests (`cargo test`) and apiserver/SDK unit tests. CI runs these on every push.
- **4–7**: pytest cases under [python/tests/integration/test_memory_limits.py](../python/tests/integration/test_memory_limits.py), tagged `@pytest.mark.gcp`. They auto-skip when `ARLEE_APISERVER` is unset, so a developer without a live cluster sees no failure. On a live cluster — typical flow: `terraform apply` → `eval "$(terraform output -raw env_setup)"` → `pytest -m gcp` from `python/` — they run end-to-end against the real Edges. **Required to pass** before merging any change to the scheduler, OOM detection, cgroup management, or substrate runtime code; otherwise on-demand.
- **9**: not codified as a runner yet. The validation-time run on the 3 default `swebench_runner --gold` instances produced an informative null (both `min=max=512` and `min=512 max=2048` got 3/3 RESOLVED — those instances don't stress memory at the 512 MiB ceiling). A meaningful replication would need a curated memory-sensitive instance subset (~50+) and a codified runner under `examples/`; deferred until someone needs the data.

Rationale for codifying 4–7 even though they don't run in CI: reproducibility (anyone can re-run with one command), structured assertions (pytest failure messages beat shell `[ $a = $b ]`), discoverability (new devs find them in `tests/integration/`), and drift detection (a behavioral regression caught months later still produces a structured failure, not "this output looks wrong"). The cost of codifying is minimal — each test is 10–20 lines of SDK calls + assertions.
