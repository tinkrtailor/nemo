# Root terraform — provisions Hetzner VPS + installs Nautiloop via module.
# For other cloud providers, see terraform/examples/.

resource "hcloud_ssh_key" "nautiloop" {
  count      = length(var.ssh_public_keys)
  name       = "nautiloop-${count.index}"
  public_key = var.ssh_public_keys[count.index]
}

resource "hcloud_server" "nautiloop" {
  name        = "nautiloop-${var.server_location}"
  server_type = var.server_type
  location    = var.server_location
  image       = "ubuntu-22.04"
  ssh_keys    = hcloud_ssh_key.nautiloop[*].id

  labels = { app = "nautiloop" }

  user_data = file("${path.module}/templates/cloud-init.yaml")
}

module "nautiloop" {
  source = "./modules/nautiloop"

  server_ip       = hcloud_server.nautiloop.ipv4_address
  ssh_private_key = file(pathexpand(var.ssh_private_key_path))
  ssh_user        = "root"

  git_repo_url         = var.git_repo_url
  git_host_token       = var.git_host_token
  repo_ssh_private_key = var.repo_ssh_private_key

  domain     = var.domain
  acme_email = var.acme_email

  control_plane_image = var.control_plane_image
  agent_base_image    = var.agent_base_image
  sidecar_image       = var.sidecar_image

  k3s_version          = var.k3s_version
  cert_manager_version = var.cert_manager_version
  postgres_password    = var.postgres_password
  postgres_volume_size = var.postgres_volume_size
  ssh_known_hosts      = var.ssh_known_hosts

  image_pull_secret_dockerconfigjson = var.image_pull_secret_dockerconfigjson

  timeouts = var.timeouts
}
