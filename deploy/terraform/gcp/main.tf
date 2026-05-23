terraform {
  required_version = ">= 1.6"

  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 6.0"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
  }
}

provider "google" {
  project = var.project_id
  region  = var.region
  zone    = var.zone
}

# ---------------------------------------------------------------------------
# Shared auth token
# ---------------------------------------------------------------------------

resource "random_password" "arlee_token" {
  length  = 48
  special = false
}

# ---------------------------------------------------------------------------
# Networking
# ---------------------------------------------------------------------------

resource "google_compute_network" "arlee" {
  name                    = "arlee-vpc"
  auto_create_subnetworks = false
}

resource "google_compute_subnetwork" "arlee" {
  name          = "arlee-subnet"
  region        = var.region
  network       = google_compute_network.arlee.id
  ip_cidr_range = "10.10.0.0/24"
}

# Allow SSH only via Identity-Aware Proxy.
resource "google_compute_firewall" "ssh_iap" {
  name    = "arlee-allow-ssh-iap"
  network = google_compute_network.arlee.name

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
  source_ranges = ["35.235.240.0/20"]
}

# Operator → Apiserver (client API).
resource "google_compute_firewall" "client_to_apiserver" {
  name    = "arlee-allow-client-apiserver"
  network = google_compute_network.arlee.name

  allow {
    protocol = "tcp"
    ports    = ["8080"]
  }
  source_ranges = [var.operator_ip_cidr]
  target_tags   = ["arlee-apiserver"]
}

# Apiserver → Edge (intra-VPC).
resource "google_compute_firewall" "apiserver_to_edge" {
  name    = "arlee-allow-apiserver-edge"
  network = google_compute_network.arlee.name

  allow {
    protocol = "tcp"
    ports    = ["8081"]
  }
  source_tags = ["arlee-apiserver"]
  target_tags = ["arlee-edge"]
}

# Edge → Apiserver (heartbeat / register, intra-VPC).
resource "google_compute_firewall" "edge_to_apiserver" {
  name    = "arlee-allow-edge-apiserver"
  network = google_compute_network.arlee.name

  allow {
    protocol = "tcp"
    ports    = ["8080"]
  }
  source_tags = ["arlee-edge"]
  target_tags = ["arlee-apiserver"]
}

# ---------------------------------------------------------------------------
# Apiserver VM
# ---------------------------------------------------------------------------

locals {
  apiserver_startup = templatefile("${path.module}/startup-script-apiserver.sh.tftpl", {
    git_repo     = var.git_repo
    git_ref      = var.git_ref
    arlee_token  = random_password.arlee_token.result
  })

  apiserver_internal_url = "http://${google_compute_instance.apiserver.network_interface[0].network_ip}:8080"
}

resource "google_compute_instance" "apiserver" {
  name         = "arlee-apiserver"
  machine_type = var.apiserver_machine_type
  zone         = var.zone
  tags         = ["arlee-apiserver"]

  boot_disk {
    initialize_params {
      image = "${var.image_project}/${var.image_family}"
      size  = 30
    }
  }

  network_interface {
    network    = google_compute_network.arlee.id
    subnetwork = google_compute_subnetwork.arlee.id
    access_config {} # ephemeral public IP
  }

  metadata = {
    startup-script = local.apiserver_startup
  }

  service_account {
    scopes = ["cloud-platform"]
  }
}

# ---------------------------------------------------------------------------
# Edge VMs
# ---------------------------------------------------------------------------

resource "google_compute_instance" "edge" {
  count        = var.edge_count
  name         = "arlee-edge-${count.index + 1}"
  machine_type = var.edge_machine_type
  zone         = var.zone
  tags         = ["arlee-edge"]

  boot_disk {
    initialize_params {
      image = "${var.image_project}/${var.image_family}"
      size  = var.edge_disk_gb
    }
  }

  network_interface {
    network    = google_compute_network.arlee.id
    subnetwork = google_compute_subnetwork.arlee.id
    access_config {} # ephemeral public IP for docker pull
  }

  metadata = {
    startup-script = templatefile("${path.module}/startup-script-edge.sh.tftpl", {
      git_repo        = var.git_repo
      git_ref         = var.git_ref
      arlee_token     = random_password.arlee_token.result
      apiserver_url   = local.apiserver_internal_url
      edge_index      = count.index + 1
    })
  }

  service_account {
    scopes = ["cloud-platform"]
  }

  # Apiserver must exist (so the Edge can register) before we boot the Edge.
  depends_on = [google_compute_instance.apiserver]
}
