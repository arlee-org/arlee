variable "project_id" {
  description = "GCP project ID"
  type        = string
  default     = "arlee-497222"
}

variable "region" {
  description = "GCP region"
  type        = string
  default     = "us-central1"
}

variable "zone" {
  description = "GCP zone within the region"
  type        = string
  default     = "us-central1-a"
}

variable "edge_count" {
  description = "Number of Edge VMs"
  type        = number
  default     = 2
}

variable "apiserver_machine_type" {
  description = "Machine type for the Apiserver VM"
  type        = string
  default     = "e2-medium"
}

variable "edge_machine_type" {
  description = "Machine type for each Edge VM"
  type        = string
  default     = "e2-standard-4"
}

variable "edge_disk_gb" {
  description = "Boot disk size on Edge VMs (Docker images can be large)"
  type        = number
  default     = 100
}

variable "image_family" {
  description = "Base image family — Ubuntu 24.04 LTS provides modern systemd + cloud-init"
  type        = string
  default     = "ubuntu-2404-lts-amd64"
}

variable "image_project" {
  description = "Project hosting the base image"
  type        = string
  default     = "ubuntu-os-cloud"
}

variable "git_repo" {
  description = "Git URL to clone arlee source from on first boot"
  type        = string
  default     = "https://github.com/arlee-org/arlee.git"
}

variable "git_ref" {
  description = "Branch/tag/commit to check out"
  type        = string
  default     = "main"
}

variable "operator_ip_cidr" {
  description = "Operator's public IP (CIDR) allowed to call the Apiserver. Use `curl -s ifconfig.me`/32."
  type        = string
}

variable "ssh_user" {
  description = "Linux user the operator logs in as (gcloud compute ssh creates it on first connect)"
  type        = string
  default     = "ubuntu"
}
