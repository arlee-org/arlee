# Arlee dev notes

Project- and dev-workflow context for working on Arlee. Auto-loaded by Claude Code at session start, and meant to serve as developer documentation for humans too.

## What this repo is

Arlee = open execution-environment infrastructure for post-training and evaluating LLM agents. Runs end-to-end on GCP (1 Apiserver + N Edge VMs); validated against SWE-bench Verified gold patches. For positioning see [README.md](README.md); for the original design + first-cut validation record see [docs/archive/v0-plan.md](docs/archive/v0-plan.md).

## Repository layout

```
Cargo.toml + crates/      Rust workspace (workspace deps centralized)
  arlee-models/           serde wire types (mirror python/arlee/models.py)
  arlee-apiserver/        axum HTTP gateway, edge registry, scheduler
  arlee-edge/             axum + bollard per-host docker driver
  arlee-cli/              clap-based `arlee` operator CLI
python/arlee/             Python SDK (httpx + pydantic)
deploy/terraform/gcp/     IaC + cloud-init for the GCP deploy
examples/                 driving workloads (swebench_runner.py is the SWE-bench demo)
docs/                     dsec.md (DeepSeek reference), archive/ (frozen design docs)
.github/workflows/        build.yml publishes binaries to the main-latest rolling release
```

Wire protocol everywhere is HTTP + JSON, auth via `X-Arlee-Token` header, intra-VPC traffic only (no TLS).

## Project conventions

- **Commit cadence.** **Batch related changes into one commit.** Don't make a separate commit per file/refactor. Group by logical change unit.
- **Debug N=1 first.** When debugging anything parallel (sandboxes, swebench tasks, rollouts), start with N=1 and only scale up after a single instance works. Avoids burning iteration cycles on identical N-way errors.
- **Don't leak dev resources into deliverable IaC.** `arlee-497222` must never be the default for any Terraform variable, never hardcoded in any startup script, never appear in `deploy/README.md` outside an explicit `$PROJECT_ID` / `your-gcp-project-id` placeholder.
- **Update docs along with code.** API change → README + CLAUDE.md in the same commit. `docs/archive/` is frozen historical; don't edit those.

## Dev GCP project

Project ID: **`arlee-497222`**. Console: <https://console.cloud.google.com/welcome?project=arlee-497222>. Billing linked, Compute Engine API enabled.

A running cluster (1× e2-medium + 2× e2-standard-4 + public IPs) costs **~$235/month** if left idle. **Tear it down at the end of each dev session**:

```bash
cd deploy/terraform/gcp
terraform destroy -auto-approve
```

Re-provisioning takes ~1–2 minutes thanks to the release-binary download flow — no cargo build on the VM.

## Standard dev commands

### Deploy / connect

```bash
cd deploy/terraform/gcp
# terraform.tfvars is gitignored. One-time setup:
#   project_id = "arlee-497222"
#   operator_ip_cidr = "$(curl -s ifconfig.me)/32"
terraform init && terraform apply
eval "$(terraform output -raw env_setup)"   # exports ARLEE_APISERVER + ARLEE_TOKEN
```

Token for ad-hoc curl: `terraform output -raw arlee_token`.

### SSH

```bash
gcloud compute ssh arlee-apiserver --zone=us-central1-a --project=arlee-497222 --tunnel-through-iap
gcloud compute ssh arlee-edge-1   --zone=us-central1-a --project=arlee-497222 --tunnel-through-iap
```

### Run the SWE-bench demo (workload runs on the apiserver VM, not laptop)

```bash
gcloud compute ssh arlee-apiserver --zone=us-central1-a --project=arlee-497222 --tunnel-through-iap --command='
TOK=$(sudo grep ARLEE_TOKEN /etc/arlee/apiserver.env | cut -d= -f2)
sudo -E env ARLEE_TOKEN=$TOK ARLEE_APISERVER=http://127.0.0.1:8080 \
  /opt/arlee-venv/bin/python /opt/arlee/examples/swebench_runner.py --gold
'
# expect: 3/3 RESOLVED
```

### Iterate Rust services without re-provisioning

Push triggers GA build (~1 min incremental). Then on the apiserver VM:

```bash
sudo curl -fsSL -o /usr/local/bin/arlee-apiserver \
  https://github.com/arlee-org/arlee/releases/download/main-latest/arlee-apiserver
sudo chmod +x /usr/local/bin/arlee-apiserver
sudo systemctl restart arlee-apiserver
```

Same on each `arlee-edge-N` VM with `/usr/local/bin/arlee-edge`.

## Known gotchas

- **Forking.** Override `var.git_repo` *and* `var.release_base_url` in your terraform.tfvars; the GA workflow must publish a `main-latest` release on your fork before VMs can pull binaries from it.
- **VM startup-script idempotency.** Both scripts gate the install behind `/var/arlee/installed`. To force re-install on an existing VM: `sudo rm /var/arlee/installed && sudo systemctl restart arlee-{apiserver,edge}`.
- **Apiserver state is in-memory.** Restart loses the sandbox→edge map. Edges re-register on next heartbeat 404; Apiserver rebuilds by querying each Edge's `/sandboxes`. Active sandboxes survive (Edge owns them).
- **Scheduler is optimistic-reservation spread-by-memory-ratio.** `pick_with_memory` atomically picks the Edge with the highest post-placement available-memory ratio (spread, not pack) and increments both `sandbox_count` and `reserved_memory_mb`; failure paths must call `release_reservation` with the same `request_min_mb`. Don't bypass this if you change the create_sandbox flow.
- **`OomEdge` ≠ `Oom` and the retry semantics differ.** `Oom` = sandbox exceeded its own `memory_max_mb` (raise the ceiling or shrink the workload). `OomEdge` = system OOM killer picked this sandbox under Edge memory pressure (the workload was fine). Retry `OomEdge` by **re-creating the sandbox** so the scheduler picks again, possibly on a different Edge — re-execing on the same sandbox just hits the same Edge under the same pressure. See [docs/memory-limits.md §3.4](docs/memory-limits.md).
- **Edge VMs require cgroup v2 + Docker `cgroupfs` driver.** Set via `/etc/docker/daemon.json` with `native.cgroupdriver=cgroupfs` *before* `apt-get install docker.io` (the postinst auto-starts dockerd; if daemon.json isn't there yet, dockerd starts with the systemd driver and `arlee-edge` later fails with "cgroup-parent for systemd cgroup should be a valid slice"). cloud-init in `deploy/terraform/gcp/startup-script-edge.sh.tftpl` does write-then-install in this order. To verify after provisioning: `docker info | grep "Cgroup Driver"` must say `cgroupfs`, not `systemd`. If wrong, `sudo systemctl restart docker && sudo systemctl restart arlee-edge` picks up the file we already wrote.
- **Don't bypass Apiserver to talk to Edges directly during dev.** Sandboxes created via direct-curl-to-Edge become invisible to Apiserver — they show up in Edge's `sandbox_count` heartbeat (skewing scheduling) but aren't killable via the Apiserver API. This bit us once during validation.
- **`gcloud compute ssh` first-connect** takes ~10s to provision the user. Patience, don't retry-loop.
- **`git` on `/opt/arlee` from the SSH user trips "dubious ownership"** because cloud-init clones it as root (uid 0) but you SSH in as a non-root user. The startup-script applies `git config --system --add safe.directory /opt/arlee` as a preemptive fix, so this only bites you on VMs provisioned before that fix landed; if so, run the same command once as root (it persists on disk) and you're good.
- **You can't `curl -o /usr/local/bin/<binary>` over a running binary** — ETXTBSY ("Failure writing output to destination"). Download to `/tmp/foo.new`, `mv` it into place (mv changes inode; the running process keeps the old one until restart).
- **Multi-account git identity:** if you maintain multiple GitHub identities, the safest way to keep them isolated for this repo is `git config --local user.name / user.email / user.signingkey` so the local file wins regardless of any `includeIf` or global config drift.

## Where to look first

- Hit a bug in API behavior? `crates/arlee-apiserver/src/api.rs` and `crates/arlee-edge/src/api.rs` are the routers.
- Hit a docker-call bug? `crates/arlee-edge/src/docker_substrate.rs` (the `SubstrateRuntime` impl for Container; see also `crates/arlee-edge/src/substrate.rs` for the trait and `crates/arlee-edge/src/edge_cgroup.rs` for per-sandbox cgroup management).
- Hit a Python ergonomics question? `python/arlee/{client,sandbox,models}.py`.
- Why-was-it-designed-this-way questions? `docs/archive/v0-plan.md` (frozen original design + validation record).
