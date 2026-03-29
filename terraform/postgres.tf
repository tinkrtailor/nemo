# FR-45: Postgres deployment as k3s pod with PVC

resource "kubernetes_persistent_volume_claim" "postgres" {
  depends_on = [kubernetes_namespace.system]

  metadata {
    name      = "nemo-postgres-data"
    namespace = "nemo-system"
  }
  spec {
    access_modes = ["ReadWriteOnce"]
    resources {
      requests = {
        storage = "20Gi"
      }
    }
  }
}

resource "kubernetes_deployment" "postgres" {
  depends_on = [
    kubernetes_persistent_volume_claim.postgres,
    kubernetes_secret.postgres_credentials,
  ]

  metadata {
    name      = "nemo-postgres"
    namespace = "nemo-system"
    labels = {
      app = "nemo-postgres"
    }
  }

  spec {
    replicas = 1

    selector {
      match_labels = {
        app = "nemo-postgres"
      }
    }

    template {
      metadata {
        labels = {
          app = "nemo-postgres"
        }
      }

      spec {
        container {
          name  = "postgres"
          image = "postgres:16-alpine"

          port {
            container_port = 5432
          }

          env {
            name  = "POSTGRES_DB"
            value = "nemo"
          }
          env {
            name  = "POSTGRES_USER"
            value = "nemo"
          }
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
            name  = "PGDATA"
            value = "/var/lib/postgresql/data/pgdata"
          }

          volume_mount {
            name       = "postgres-data"
            mount_path = "/var/lib/postgresql/data"
          }

          resources {
            requests = {
              cpu    = "250m"
              memory = "512Mi"
            }
            limits = {
              cpu    = "1000m"
              memory = "2Gi"
            }
          }

          liveness_probe {
            exec {
              command = ["pg_isready", "-U", "nemo"]
            }
            initial_delay_seconds = 15
            period_seconds        = 10
          }

          readiness_probe {
            exec {
              command = ["pg_isready", "-U", "nemo"]
            }
            initial_delay_seconds = 5
            period_seconds        = 5
          }
        }

        volume {
          name = "postgres-data"
          persistent_volume_claim {
            claim_name = "nemo-postgres-data"
          }
        }
      }
    }
  }
}

# FR-56: Postgres Service on port 5432
resource "kubernetes_service" "postgres" {
  depends_on = [kubernetes_deployment.postgres]

  metadata {
    name      = "nemo-postgres"
    namespace = "nemo-system"
  }

  spec {
    selector = {
      app = "nemo-postgres"
    }

    port {
      port        = 5432
      target_port = 5432
    }
  }
}

# FR-55: Daily pg_dump CronJob
resource "kubernetes_cron_job_v1" "postgres_backup" {
  depends_on = [kubernetes_deployment.postgres]

  metadata {
    name      = "nemo-postgres-backup"
    namespace = "nemo-system"
  }

  spec {
    schedule = "0 2 * * *" # Daily at 2 AM

    job_template {
      metadata {}
      spec {
        template {
          metadata {}
          spec {
            container {
              name  = "backup"
              image = "postgres:16-alpine"

              command = ["/bin/sh", "-c"]
              args = [<<-EOT
                set -e
                # Delete backups older than 7 days
                find /data/backups -name "nemo-*.sql.gz" -mtime +7 -delete 2>/dev/null || true
                # Create new backup
                PGPASSWORD="$POSTGRES_PASSWORD" pg_dump -h nemo-postgres -U nemo nemo | \
                  gzip > /data/backups/nemo-$(date +%Y%m%d-%H%M%S).sql.gz
                echo "Backup completed successfully"
              EOT
              ]

              env {
                name = "POSTGRES_PASSWORD"
                value_from {
                  secret_key_ref {
                    name = "nemo-postgres-credentials"
                    key  = "password"
                  }
                }
              }

              volume_mount {
                name       = "backup-volume"
                mount_path = "/data/backups"
              }
            }

            volume {
              name = "backup-volume"
              host_path {
                path = "/data/backups"
                type = "DirectoryOrCreate"
              }
            }

            restart_policy = "OnFailure"
          }
        }
      }
    }
  }
}
