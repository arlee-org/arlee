# Deploying Arlee v0 to GCP

The Terraform module in `terraform/gcp/` provisions an Arlee v0 cluster into a
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
- The Arlee repo accessible at the URL given by `var.git_repo` (default
  `https://github.com/arlee-org/arlee.git`, `main` branch). The VMs clone
  it on first boot and run `cargo build --release`. If you forked the
  repo, override `git_repo` to point at your fork.

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

First boot takes ~5–10 minutes because each VM compiles the Rust binaries
from source. The `apply` step itself returns once VMs exist; readiness is
deferred to `systemctl` on the VMs.

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
