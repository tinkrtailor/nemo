# Control plane: nemo.toml config, repo-init job, API server, loop engine

resource "kubernetes_config_map" "nemo_config" {
  depends_on = [kubernetes_namespace.system]

  metadata {
    name      = "nemo-config"
    namespace = "nemo-system"
  }
  data = {
    "nemo.toml" = <<-EOT
      [cluster]
      git_repo_url = "${var.git_repo_url}"
      agent_image = "${var.agent_base_image}"
      sidecar_image = "${var.sidecar_image}"
      ${var.image_pull_secret_dockerconfigjson != null ? "image_pull_secret = \"nemo-registry-creds\"" : ""}
    EOT
  }
}

# --- Repo Init Job ---

resource "kubernetes_job" "repo_init" {
  depends_on = [
    kubernetes_persistent_volume_claim.bare_repo,
    kubernetes_config_map.cluster_config,
    kubernetes_config_map.ssh_known_hosts,
    kubernetes_secret.repo_ssh_key,
    null_resource.ssh_keyscan,
  ]

  metadata {
    name      = "nemo-repo-init"
    namespace = "nemo-system"
  }

  spec {
    backoff_limit = 3

    template {
      metadata {}
      spec {
        security_context {
          run_as_user  = 1000
          run_as_group = 1000
          fs_group     = 1000
        }

        container {
          name  = "repo-init"
          image = "alpine/git:latest"

          command = ["/bin/sh", "-c"]
          args = [<<-EOT
            set -e
            if [ ! -e /bare-repo/HEAD ]; then
              git init --bare /bare-repo
            fi
            git -C /bare-repo remote remove origin 2>/dev/null || true
            git -C /bare-repo remote add origin "$GIT_REPO_URL"
            mkdir -p "$HOME/.ssh"
            cp /secrets/ssh-key/id_ed25519 "$HOME/.ssh/id_ed25519"
            chmod 600 "$HOME/.ssh/id_ed25519"
            cp /secrets/ssh-known-hosts/known_hosts "$HOME/.ssh/known_hosts"
            git -C /bare-repo fetch --all
          EOT
          ]

          env {
            name  = "HOME"
            value = "/tmp"
          }
          env {
            name = "GIT_REPO_URL"
            value_from {
              config_map_key_ref {
                name = "nemo-cluster-config"
                key  = "git_repo_url"
              }
            }
          }

          volume_mount {
            name       = "bare-repo"
            mount_path = "/bare-repo"
          }
          volume_mount {
            name       = "ssh-key"
            mount_path = "/secrets/ssh-key"
            read_only  = true
          }
          volume_mount {
            name       = "ssh-known-hosts"
            mount_path = "/secrets/ssh-known-hosts"
            read_only  = true
          }
        }

        volume {
          name = "bare-repo"
          persistent_volume_claim { claim_name = "nemo-bare-repo" }
        }
        volume {
          name = "ssh-key"
          secret {
            secret_name  = "nemo-repo-ssh-key"
            default_mode = "0444"
          }
        }
        volume {
          name = "ssh-known-hosts"
          config_map { name = "nemo-ssh-known-hosts" }
        }

        restart_policy = "OnFailure"
      }
    }
  }

  wait_for_completion = true
  timeouts { create = "10m" }
}

# --- API Server ---

resource "kubernetes_deployment" "api_server" {
  depends_on = [
    kubernetes_service.postgres,
    kubernetes_secret.api_key,
    kubernetes_secret.git_host_token,
    kubernetes_service_account.api_server,
    kubernetes_persistent_volume_claim.bare_repo,
    kubernetes_job.repo_init,
    kubernetes_config_map.nemo_config,
    kubernetes_secret.registry_creds_system,
  ]

  metadata {
    name      = "nemo-api-server"
    namespace = "nemo-system"
    labels    = { app = "nemo-api-server" }
  }

  spec {
    replicas = 1

    selector { match_labels = { app = "nemo-api-server" } }

    template {
      metadata {
        labels = { app = "nemo-api-server" }
        annotations = {
          "config-checksum" = sha256(kubernetes_config_map.nemo_config.data["nemo.toml"])
        }
      }

      spec {
        service_account_name = "nemo-api-server"

        dynamic "image_pull_secrets" {
          for_each = var.image_pull_secret_dockerconfigjson != null ? [1] : []
          content { name = "nemo-registry-creds" }
        }

        security_context { fs_group = 1000 }

        container {
          name  = "api-server"
          image = var.control_plane_image
          args  = ["api-server"]

          port { container_port = 8080 }

          env {
            name = "DATABASE_URL"
            value_from {
              secret_key_ref {
                name = "nemo-postgres-credentials"
                key  = "DATABASE_URL"
              }
            }
          }
          env {
            name = "NEMO_API_KEY"
            value_from {
              secret_key_ref {
                name = "nemo-api-key"
                key  = "NEMO_API_KEY"
              }
            }
          }
          env {
            name = "GIT_HOST_TOKEN"
            value_from {
              secret_key_ref {
                name = "nemo-git-host-token"
                key  = "GIT_HOST_TOKEN"
              }
            }
          }
          env {
            name  = "BARE_REPO_PATH"
            value = "/bare-repo"
          }

          resources {
            requests = { cpu = "100m", memory = "256Mi" }
            limits   = { cpu = "500m", memory = "512Mi" }
          }

          volume_mount {
            name       = "bare-repo"
            mount_path = "/bare-repo"
          }
          volume_mount {
            name       = "nemo-config"
            mount_path = "/etc/nemo"
            read_only  = true
          }

          startup_probe {
            http_get {
              path = "/health"
              port = 8080
            }
            failure_threshold = 30
            period_seconds    = 2
            timeout_seconds   = 3
          }

          liveness_probe {
            tcp_socket { port = 8080 }
            period_seconds  = 15
            timeout_seconds = 3
          }

          readiness_probe {
            http_get {
              path = "/health"
              port = 8080
            }
            period_seconds  = 10
            timeout_seconds = 3
          }
        }

        volume {
          name = "bare-repo"
          persistent_volume_claim { claim_name = "nemo-bare-repo" }
        }
        volume {
          name = "nemo-config"
          config_map { name = "nemo-config" }
        }
      }
    }
  }
}

resource "kubernetes_service" "api_server" {
  depends_on = [kubernetes_deployment.api_server]

  metadata {
    name      = "nemo-api-server"
    namespace = "nemo-system"
  }
  spec {
    selector = { app = "nemo-api-server" }
    port {
      port        = 8080
      target_port = 8080
    }
  }
}

# --- Loop Engine ---

resource "kubernetes_deployment" "loop_engine" {
  depends_on = [
    kubernetes_service.postgres,
    kubernetes_secret.api_key,
    kubernetes_secret.git_host_token,
    kubernetes_service_account.loop_engine,
    kubernetes_persistent_volume_claim.bare_repo,
    kubernetes_job.repo_init,
    kubernetes_config_map.nemo_config,
    kubernetes_secret.registry_creds_system,
  ]

  metadata {
    name      = "nemo-loop-engine"
    namespace = "nemo-system"
    labels    = { app = "nemo-loop-engine" }
  }

  spec {
    replicas = 1

    selector { match_labels = { app = "nemo-loop-engine" } }

    template {
      metadata {
        labels = { app = "nemo-loop-engine" }
        annotations = {
          "config-checksum" = sha256(kubernetes_config_map.nemo_config.data["nemo.toml"])
        }
      }

      spec {
        service_account_name = "nemo-loop-engine"

        dynamic "image_pull_secrets" {
          for_each = var.image_pull_secret_dockerconfigjson != null ? [1] : []
          content { name = "nemo-registry-creds" }
        }

        security_context { fs_group = 1000 }

        container {
          name  = "loop-engine"
          image = var.control_plane_image
          args  = ["loop-engine"]

          env {
            name = "DATABASE_URL"
            value_from {
              secret_key_ref {
                name = "nemo-postgres-credentials"
                key  = "DATABASE_URL"
              }
            }
          }
          env {
            name = "GIT_HOST_TOKEN"
            value_from {
              secret_key_ref {
                name = "nemo-git-host-token"
                key  = "GIT_HOST_TOKEN"
              }
            }
          }
          env {
            name  = "BARE_REPO_PATH"
            value = "/bare-repo"
          }
          env {
            name  = "AGENT_IMAGE"
            value = var.agent_base_image
          }

          volume_mount {
            name       = "bare-repo"
            mount_path = "/bare-repo"
          }
          volume_mount {
            name       = "nemo-config"
            mount_path = "/etc/nemo"
            read_only  = true
          }

          resources {
            requests = { cpu = "100m", memory = "256Mi" }
            limits   = { cpu = "500m", memory = "512Mi" }
          }

          liveness_probe {
            exec { command = ["kill", "-0", "1"] }
            initial_delay_seconds = 15
            period_seconds        = 30
            timeout_seconds       = 3
          }
        }

        volume {
          name = "bare-repo"
          persistent_volume_claim { claim_name = "nemo-bare-repo" }
        }
        volume {
          name = "nemo-config"
          config_map { name = "nemo-config" }
        }
      }
    }
  }
}
