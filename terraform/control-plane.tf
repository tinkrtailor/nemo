# Control plane runtime config — mounted as /etc/nemo/nemo.toml in both deployments.
# The binary loads this via its fallback path when ./nemo.toml doesn't exist.
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
    EOT
  }
}

# FR-46: Control plane Deployments (API server + Loop engine)

# FR-46: API Server Deployment
resource "kubernetes_deployment" "api_server" {
  depends_on = [
    kubernetes_service.postgres,
    kubernetes_secret.api_key,
    kubernetes_secret.git_host_token,
    kubernetes_service_account.api_server,
    kubernetes_persistent_volume_claim.bare_repo,
    kubernetes_job.repo_init,
    kubernetes_config_map.nemo_config,
  ]

  metadata {
    name      = "nemo-api-server"
    namespace = "nemo-system"
    labels = {
      app = "nemo-api-server"
    }
  }

  spec {
    replicas = 1

    selector {
      match_labels = {
        app = "nemo-api-server"
      }
    }

    template {
      metadata {
        labels = {
          app = "nemo-api-server"
        }
      }

      spec {
        service_account_name = "nemo-api-server"

        security_context {
          fs_group = 1000
        }

        container {
          name  = "api-server"
          image = var.control_plane_image

          args = ["api-server"]

          port {
            container_port = 8080
          }

          # FR-56: Database URL from Secret (Finding 8: K8s does not shell-expand env refs)
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

          resources {
            requests = {
              cpu    = "100m"
              memory = "256Mi"
            }
            limits = {
              cpu    = "500m"
              memory = "512Mi"
            }
          }

          env {
            name  = "BARE_REPO_PATH"
            value = "/bare-repo"
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

          # Liveness: lightweight TCP check — don't consume DB pool connections.
          # Only restarts pod if the process is completely dead.
          liveness_probe {
            tcp_socket {
              port = 8080
            }
            period_seconds  = 15
            timeout_seconds = 3
          }

          # Readiness: deep check via /health (verifies Postgres).
          # Removes pod from service if DB is unreachable.
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
          persistent_volume_claim {
            claim_name = "nemo-bare-repo"
          }
        }

        volume {
          name = "nemo-config"
          config_map {
            name = "nemo-config"
          }
        }
      }
    }
  }
}

# API Server Service
resource "kubernetes_service" "api_server" {
  depends_on = [kubernetes_deployment.api_server]

  metadata {
    name      = "nemo-api-server"
    namespace = "nemo-system"
  }

  spec {
    selector = {
      app = "nemo-api-server"
    }

    port {
      port        = 8080
      target_port = 8080
    }
  }
}

# FR-46: Loop Engine Deployment
resource "kubernetes_deployment" "loop_engine" {
  depends_on = [
    kubernetes_service.postgres,
    kubernetes_secret.api_key,
    kubernetes_secret.git_host_token,
    kubernetes_service_account.loop_engine,
    kubernetes_persistent_volume_claim.bare_repo,
    kubernetes_job.repo_init,
    kubernetes_config_map.nemo_config,
  ]

  metadata {
    name      = "nemo-loop-engine"
    namespace = "nemo-system"
    labels = {
      app = "nemo-loop-engine"
    }
  }

  spec {
    replicas = 1

    selector {
      match_labels = {
        app = "nemo-loop-engine"
      }
    }

    template {
      metadata {
        labels = {
          app = "nemo-loop-engine"
        }
      }

      spec {
        service_account_name = "nemo-loop-engine"

        security_context {
          fs_group = 1000
        }

        container {
          name  = "loop-engine"
          image = var.control_plane_image

          args = ["loop-engine"]

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
            name = "NEMO_API_KEY"
            value_from {
              secret_key_ref {
                name = "nemo-api-key"
                key  = "NEMO_API_KEY"
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
            requests = {
              cpu    = "100m"
              memory = "256Mi"
            }
            limits = {
              cpu    = "500m"
              memory = "512Mi"
            }
          }

          # Liveness: verify the process is still alive and responsive.
          # The loop engine doesn't serve HTTP, so use exec-based check.
          # kill -0 checks PID 1 (tini) is alive without sending a signal.
          liveness_probe {
            exec {
              command = ["kill", "-0", "1"]
            }
            initial_delay_seconds = 15
            period_seconds        = 30
            timeout_seconds       = 3
          }
        }

        volume {
          name = "bare-repo"
          persistent_volume_claim {
            claim_name = "nemo-bare-repo"
          }
        }

        volume {
          name = "nemo-config"
          config_map {
            name = "nemo-config"
          }
        }
      }
    }
  }
}

# FR-47: Repo init Job
resource "kubernetes_job" "repo_init" {
  depends_on = [
    kubernetes_persistent_volume_claim.bare_repo,
    kubernetes_config_map.cluster_config,
    kubernetes_config_map.ssh_known_hosts,
    kubernetes_secret.repo_ssh_key,
    # Finding 12: Wait for ssh-keyscan fallback to complete before fetching
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
            if [ ! -d /bare-repo/HEAD ]; then
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

          # HOME=/tmp so non-root UID 1000 can write ~/.ssh on alpine/git
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
          persistent_volume_claim {
            claim_name = "nemo-bare-repo"
          }
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
          config_map {
            name = "nemo-ssh-known-hosts"
          }
        }

        restart_policy = "OnFailure"
      }
    }
  }

  wait_for_completion = true
  timeouts {
    create = "10m"
  }
}
