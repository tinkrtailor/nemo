# FR-46: Control plane Deployments (API server + Loop engine)

# FR-46: API Server Deployment
resource "kubernetes_deployment" "api_server" {
  depends_on = [
    kubernetes_service.postgres,
    kubernetes_secret.api_key,
    kubernetes_secret.git_host_token,
    kubernetes_service_account.api_server,
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

        container {
          name  = "api-server"
          image = var.control_plane_image

          args = ["api-server"]

          port {
            container_port = 8080
          }

          # FR-56: Database URL with K8s env var composition
          env {
            name = "POSTGRES_PASSWORD"
            value_from {
              secret_key_ref {
                name = "nemo-postgres-credentials"
                key  = "password"
              }
            }
          }
          env {
            name  = "DATABASE_URL"
            value = "postgres://nemo:$(POSTGRES_PASSWORD)@nemo-postgres:5432/nemo"
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

          liveness_probe {
            http_get {
              path = "/health"
              port = 8080
            }
            initial_delay_seconds = 10
            period_seconds        = 15
          }

          readiness_probe {
            http_get {
              path = "/health"
              port = 8080
            }
            initial_delay_seconds = 5
            period_seconds        = 5
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

        container {
          name  = "loop-engine"
          image = var.control_plane_image

          args = ["loop-engine"]

          env {
            name = "POSTGRES_PASSWORD"
            value_from {
              secret_key_ref {
                name = "nemo-postgres-credentials"
                key  = "password"
              }
            }
          }
          env {
            name  = "DATABASE_URL"
            value = "postgres://nemo:$(POSTGRES_PASSWORD)@nemo-postgres:5432/nemo"
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
        }

        volume {
          name = "bare-repo"
          persistent_volume_claim {
            claim_name = "nemo-bare-repo"
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
            mkdir -p /root/.ssh
            cp /secrets/ssh-key/id_ed25519 /root/.ssh/id_ed25519
            chmod 600 /root/.ssh/id_ed25519
            cp /secrets/ssh-known-hosts/known_hosts /root/.ssh/known_hosts
            git -C /bare-repo fetch --all
          EOT
          ]

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
            default_mode = "0600"
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

  wait_for_completion = false
}
