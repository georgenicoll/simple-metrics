output "url" {
  description = "Where the page is served."
  value       = "http://${var.http_bind_address == "0.0.0.0" ? var.host : var.http_bind_address}:${var.http_port}/"
}

output "logs_hint" {
  description = "How to see what the containers are saying."
  value       = "ssh ${var.ssh_user}@${var.host} docker logs --tail 50 ${docker_container.collector.name} ${docker_container.web.name}"
}
