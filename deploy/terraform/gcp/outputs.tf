output "apiserver_ip" {
  description = "Public IP of the Apiserver VM"
  value       = google_compute_instance.apiserver.network_interface[0].access_config[0].nat_ip
}

output "apiserver_internal_ip" {
  description = "Internal IP of the Apiserver (used by Edges)"
  value       = google_compute_instance.apiserver.network_interface[0].network_ip
}

output "edge_ips" {
  description = "Public IPs of the Edge VMs"
  value = [
    for inst in google_compute_instance.edge :
    inst.network_interface[0].access_config[0].nat_ip
  ]
}

output "arlee_token" {
  description = "Shared auth token. Export as ARLEE_TOKEN to talk to the cluster."
  value       = random_password.arlee_token.result
  sensitive   = true
}

output "env_setup" {
  description = "Source this with `eval $(terraform output -raw env_setup)` from the operator's shell."
  value       = <<-EOT
    export ARLEE_APISERVER='http://${google_compute_instance.apiserver.network_interface[0].access_config[0].nat_ip}:8080'
    export ARLEE_TOKEN='${random_password.arlee_token.result}'
  EOT
  sensitive   = true
}
