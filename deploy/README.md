# Deploying Arlee v0 to GCP

The Terraform module in `terraform/gcp/` provisions an Arlee v0 cluster into a
single GCP project: 1 Apiserver VM + N Edge VMs (default N=2), in one VPC.

## Prerequisites

- `gcloud` authenticated to the target project (`gcloud auth application-default login`).
- A GCP project with billing enabled (default in `variables.tf`: `arlee-497222`).
- The Compute Engine API enabled:
  ```
  gcloud services enable compute.googleapis.com --project=arlee-497222
  ```
- `terraform >= 1.6`.
- The repo pushed to the Git URL configured in `git_repo` (default
  `https://github.com/arlee-org/arlee.git`, `main` branch). The VMs clone this
  on first boot and run `cargo build --release`.

## 5-minute deploy

```bash
cd deploy/terraform/gcp
cp terraform.tfvars.example terraform.tfvars
# Edit terraform.tfvars: at minimum set operator_ip_cidr to your /32.
#   curl -s https://ifconfig.me     ← your public IP

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
gcloud compute ssh arlee-apiserver --zone=us-central1-a --project=arlee-497222

# On the VM:
cd /opt/arlee
python3 examples/swebench_runner.py --gold --n 3
# expect: 3/3 PASS
```

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
gcloud compute ssh arlee-apiserver --zone=us-central1-a --project=arlee-497222 \
    --command='sudo journalctl -u arlee-apiserver -f'

# Edge logs
gcloud compute ssh arlee-edge-1 --zone=us-central1-a --project=arlee-497222 \
    --command='sudo journalctl -u arlee-edge -f'

# First-boot script output (where build failures show up)
gcloud compute ssh arlee-edge-1 --zone=us-central1-a --project=arlee-497222 \
    --command='sudo cat /var/log/syslog | grep startup-script'
```
