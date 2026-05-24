# Deploying Arlee to GCP

The Terraform module in `terraform/gcp/` provisions an Arlee cluster into a
single GCP project: 1 Apiserver VM + N Edge VMs (default N=2), in one VPC.

> **What this module is — and isn't.** This is the *deliverable* IaC: it
> deploys Arlee into **your** GCP project. It has no hardcoded defaults
> that target ours. Defaults that aren't safely neutral (e.g. `project_id`)
> are required variables; you must supply them via `terraform.tfvars` or
> `-var`. Anything in `terraform.tfvars` is gitignored.

## Prerequisites

- A GCP project of your own, with billing enabled. Pick its ID; the rest of
  this guide refers to it as `$PROJECT_ID`.
- `gcloud` authenticated against that project:
  ```bash
  gcloud auth application-default login
  ```
- The Compute Engine API enabled on it:
  ```bash
  gcloud services enable compute.googleapis.com --project=$PROJECT_ID
  ```
- `terraform >= 1.6`.
- Pre-built `arlee-apiserver` and `arlee-edge` binaries accessible at the
  URL given by `var.release_base_url` (default
  `https://github.com/arlee-org/arlee/releases/download/main-latest`,
  built by `.github/workflows/build.yml` on every push to main). The VMs
  `curl` them on first boot. If you fork, push your branch so the workflow
  publishes a release on your repo, then override `release_base_url`
  (and `git_repo`, which still gets cloned on the Apiserver VM for the
  Python SDK source and `examples/`).

### Edge VM requirements

Edge VMs need cgroup v2 + Docker's `cgroupfs` cgroup driver so per-sandbox
memory limits are honored as hard `memory.min` / `memory.max` reservations
(see [docs/memory-limits.md §4.2](../docs/memory-limits.md)).
The cloud-init in this module sets both:

- The image (Ubuntu 22.04+) ships with cgroup v2 mounted at
  `/sys/fs/cgroup`. Verify with `mount | grep cgroup2` on the VM.
- `/etc/docker/daemon.json` is written with
  `{"exec-opts": ["native.cgroupdriver=cgroupfs"]}` before dockerd starts.
  Docker's modern default is `systemd`; we override because our cgroup
  manager uses path-style `--cgroup-parent=/arlee/<sandbox_id>`, not
  systemd slice names.

If you fork this module and write your own cloud-init, both of these are
non-optional. Without them, `arlee-edge` fails to start (it fails fast
rather than silently dropping the memory limit semantics).

## 5-minute deploy

```bash
cd deploy/terraform/gcp
cp terraform.tfvars.example terraform.tfvars
# Edit terraform.tfvars:
#   - project_id      = "$PROJECT_ID"          (your GCP project)
#   - operator_ip_cidr = "$(curl -s ifconfig.me)/32"

terraform init
terraform apply
```

First boot takes ~30–90 seconds: each VM does an `apt-get install` and
a `curl` of the release binary, then systemd starts the service. The
`apply` step itself returns once the VMs exist; readiness is deferred to
`systemctl` on the VMs, so give it a minute before `arlee health`.

## Connect

```bash
# Pull the auth env into your shell:
eval "$(terraform output -raw env_setup)"

# Verify the cluster is up (this command will fail until the VMs finish their
# startup-script — give it a few minutes after `apply` returns).
arlee health
arlee edges   # both Edges should show healthy=✓
```

## Run the SWE-bench demo

```bash
# SSH into the Apiserver VM (workload runs there, not on your laptop).
gcloud compute ssh arlee-apiserver \
    --zone=us-central1-a --project=$PROJECT_ID --tunnel-through-iap

# On the VM — runner uses the venv that the startup-script set up at
# /opt/arlee-venv, which has the SDK and `swebench` package pre-installed.
sudo /opt/arlee-venv/bin/python /opt/arlee/examples/swebench_runner.py --gold
# expect: 3/3 RESOLVED
```

The runner needs `ARLEE_APISERVER` and `ARLEE_TOKEN` in its environment.
Easy: `source /etc/arlee/apiserver.env` before running, then export
`ARLEE_APISERVER=http://127.0.0.1:8080`.

Pull trajectories back to your laptop:

```bash
arlee sandboxes                # list sandbox IDs
arlee logs <sandbox-id> --download ./traj/<sandbox-id>.jsonl
```

## Teardown

```bash
terraform destroy
```

## What the deployment contains

| Resource | Purpose |
|---|---|
| `arlee-vpc` + `arlee-subnet` (10.10.0.0/24) | Isolated network |
| `arlee-apiserver` VM (e2-medium) | Runs `arlee-apiserver` systemd unit |
| `arlee-edge-1..N` VMs (e2-standard-4) | Run `arlee-edge` + Docker daemon |
| Firewall: `arlee-allow-ssh-iap` | SSH only via Identity-Aware Proxy |
| Firewall: `arlee-allow-client-apiserver` | Operator IP → Apiserver:8080 |
| Firewall: `arlee-allow-apiserver-edge` | Apiserver → Edge:8081 |
| Firewall: `arlee-allow-edge-apiserver` | Edge → Apiserver:8080 (register/heartbeat) |
| `random_password.arlee_token` | Shared auth secret, surfaced via `terraform output` |

## Debugging

```bash
# Apiserver logs
gcloud compute ssh arlee-apiserver --zone=us-central1-a --project=$PROJECT_ID \
    --command='sudo journalctl -u arlee-apiserver -f'

# Edge logs
gcloud compute ssh arlee-edge-1 --zone=us-central1-a --project=$PROJECT_ID \
    --command='sudo journalctl -u arlee-edge -f'

# First-boot script output (where build failures show up)
gcloud compute ssh arlee-edge-1 --zone=us-central1-a --project=$PROJECT_ID \
    --command='sudo cat /var/log/syslog | grep startup-script'
```
