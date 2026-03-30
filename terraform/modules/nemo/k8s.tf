# Kubernetes resources: namespaces, storage, secrets, RBAC, networking

# --- Namespaces ---

resource "kubernetes_namespace" "system" {
  depends_on = [null_resource.kubeconfig]

  metadata {
    name   = "nemo-system"
    labels = { app = "nemo" }
  }
}

resource "kubernetes_namespace" "jobs" {
  depends_on = [null_resource.kubeconfig]

  metadata {
    name   = "nemo-jobs"
    labels = { app = "nemo" }
  }
}

# --- Shared bare repo storage ---
# Two PVs pointing to the same hostPath — one per namespace PVC.
# Single-node k3s, so hostPath is safe.

resource "kubernetes_persistent_volume" "bare_repo_system" {
  depends_on = [null_resource.kubeconfig]

  metadata { name = "nemo-bare-repo-system" }
  spec {
    capacity                         = { storage = "100Gi" }
    access_modes                     = ["ReadWriteMany"]
    persistent_volume_reclaim_policy = "Retain"
    storage_class_name               = "manual"
    persistent_volume_source {
      host_path {
        path = "/data/nemo-bare-repo"
        type = "DirectoryOrCreate"
      }
    }
  }
}

resource "kubernetes_persistent_volume" "bare_repo_jobs" {
  depends_on = [null_resource.kubeconfig]

  metadata { name = "nemo-bare-repo-jobs" }
  spec {
    capacity                         = { storage = "100Gi" }
    access_modes                     = ["ReadWriteMany"]
    persistent_volume_reclaim_policy = "Retain"
    storage_class_name               = "manual"
    persistent_volume_source {
      host_path {
        path = "/data/nemo-bare-repo"
        type = "DirectoryOrCreate"
      }
    }
  }
}

resource "kubernetes_persistent_volume_claim" "bare_repo" {
  depends_on = [kubernetes_namespace.system, kubernetes_persistent_volume.bare_repo_system]

  metadata {
    name      = "nemo-bare-repo"
    namespace = "nemo-system"
  }
  spec {
    access_modes       = ["ReadWriteMany"]
    storage_class_name = "manual"
    volume_name        = kubernetes_persistent_volume.bare_repo_system.metadata[0].name
    resources { requests = { storage = "100Gi" } }
  }
}

resource "kubernetes_persistent_volume_claim" "bare_repo_jobs" {
  depends_on = [kubernetes_namespace.jobs, kubernetes_persistent_volume.bare_repo_jobs]

  metadata {
    name      = "nemo-bare-repo"
    namespace = "nemo-jobs"
  }
  spec {
    access_modes       = ["ReadWriteMany"]
    storage_class_name = "manual"
    volume_name        = kubernetes_persistent_volume.bare_repo_jobs.metadata[0].name
    resources { requests = { storage = "100Gi" } }
  }
}

# Session state PVC
resource "kubernetes_persistent_volume_claim" "sessions" {
  depends_on = [kubernetes_namespace.jobs]

  metadata {
    name      = "nemo-sessions"
    namespace = "nemo-jobs"
  }
  spec {
    access_modes = ["ReadWriteOnce"]
    resources { requests = { storage = "10Gi" } }
  }
}

# --- Secrets ---

resource "kubernetes_secret" "postgres_credentials" {
  depends_on = [kubernetes_namespace.system]

  metadata {
    name      = "nemo-postgres-credentials"
    namespace = "nemo-system"
  }
  data = {
    password     = local.postgres_password
    DATABASE_URL = "postgres://nemo:${local.postgres_password}@nemo-postgres:5432/nemo"
  }
}

resource "kubernetes_secret" "api_key" {
  depends_on = [kubernetes_namespace.system]

  metadata {
    name      = "nemo-api-key"
    namespace = "nemo-system"
  }
  data = { NEMO_API_KEY = random_password.api_key.result }
}

resource "kubernetes_secret" "git_host_token" {
  depends_on = [kubernetes_namespace.system]

  metadata {
    name      = "nemo-git-host-token"
    namespace = "nemo-system"
  }
  data = { GIT_HOST_TOKEN = var.git_host_token }
}

resource "kubernetes_secret" "repo_ssh_key" {
  depends_on = [kubernetes_namespace.system]

  metadata {
    name      = "nemo-repo-ssh-key"
    namespace = "nemo-system"
  }
  data = { id_ed25519 = local.deploy_private_key }
}

resource "kubernetes_secret" "registry_creds" {
  count      = var.image_pull_secret_dockerconfigjson != null ? 1 : 0
  depends_on = [kubernetes_namespace.jobs]

  metadata {
    name      = "nemo-registry-creds"
    namespace = "nemo-jobs"
  }
  type = "kubernetes.io/dockerconfigjson"
  data = { ".dockerconfigjson" = var.image_pull_secret_dockerconfigjson }
}

resource "kubernetes_secret" "registry_creds_system" {
  count      = var.image_pull_secret_dockerconfigjson != null ? 1 : 0
  depends_on = [kubernetes_namespace.system]

  metadata {
    name      = "nemo-registry-creds"
    namespace = "nemo-system"
  }
  type = "kubernetes.io/dockerconfigjson"
  data = { ".dockerconfigjson" = var.image_pull_secret_dockerconfigjson }
}

# --- ConfigMaps ---

resource "kubernetes_config_map" "cluster_config" {
  depends_on = [kubernetes_namespace.system]

  metadata {
    name      = "nemo-cluster-config"
    namespace = "nemo-system"
  }
  data = {
    git_repo_url = var.git_repo_url
    domain       = local.has_domain ? var.domain : var.server_ip
  }
}

resource "kubernetes_config_map" "ssh_known_hosts" {
  depends_on = [kubernetes_namespace.system]

  metadata {
    name      = "nemo-ssh-known-hosts"
    namespace = "nemo-system"
  }
  data = { known_hosts = var.ssh_known_hosts }
}

resource "kubernetes_config_map" "ssh_known_hosts_jobs" {
  depends_on = [kubernetes_namespace.jobs]

  metadata {
    name      = "nemo-ssh-known-hosts"
    namespace = "nemo-jobs"
  }
  data = { known_hosts = var.ssh_known_hosts }
}

# Fallback ssh-keyscan if ssh_known_hosts not provided
resource "null_resource" "ssh_keyscan" {
  count = var.ssh_known_hosts == "" ? 1 : 0

  depends_on = [kubernetes_config_map.ssh_known_hosts]

  triggers = {
    git_repo_url = var.git_repo_url
  }

  provisioner "local-exec" {
    command = <<-EOT
      GITHOST=$(echo "$GIT_REPO_URL" | sed -E 's/.*@([^:]+):.*/\1/' | sed -E 's|https?://([^/]+).*|\1|')
      KNOWN_HOSTS=$(ssh-keyscan "$GITHOST" 2>/dev/null)
      if [ -z "$KNOWN_HOSTS" ]; then
        echo "ERROR: ssh-keyscan returned empty for $GITHOST" >&2
        exit 1
      fi
      for NS in nemo-system nemo-jobs; do
        kubectl --kubeconfig ${local.kubeconfig_path} -n "$NS" \
          create configmap nemo-ssh-known-hosts \
          --from-literal="known_hosts=$KNOWN_HOSTS" \
          --dry-run=client -o yaml | \
          kubectl --kubeconfig ${local.kubeconfig_path} apply -f -
      done
    EOT

    environment = {
      GIT_REPO_URL = var.git_repo_url
    }
  }
}

# --- RBAC ---

resource "kubernetes_service_account" "loop_engine" {
  depends_on = [kubernetes_namespace.system]
  metadata {
    name      = "nemo-loop-engine"
    namespace = "nemo-system"
  }
}

resource "kubernetes_service_account" "api_server" {
  depends_on = [kubernetes_namespace.system]
  metadata {
    name      = "nemo-api-server"
    namespace = "nemo-system"
  }
}

resource "kubernetes_role" "loop_engine_jobs" {
  depends_on = [kubernetes_namespace.jobs]

  metadata {
    name      = "nemo-loop-engine"
    namespace = "nemo-jobs"
  }

  rule {
    api_groups = ["batch"]
    resources  = ["jobs"]
    verbs      = ["create", "delete", "list", "watch", "get"]
  }
  rule {
    api_groups = [""]
    resources  = ["pods"]
    verbs      = ["list", "get"]
  }
  rule {
    api_groups = [""]
    resources  = ["pods/log"]
    verbs      = ["get"]
  }
  rule {
    api_groups = [""]
    resources  = ["secrets"]
    verbs      = ["create", "update", "get"]
  }
  rule {
    api_groups = [""]
    resources  = ["configmaps"]
    verbs      = ["create", "update", "get"]
  }
  rule {
    api_groups = [""]
    resources  = ["persistentvolumeclaims"]
    verbs      = ["get", "list"]
  }
}

resource "kubernetes_role_binding" "loop_engine_jobs" {
  depends_on = [kubernetes_role.loop_engine_jobs, kubernetes_service_account.loop_engine]

  metadata {
    name      = "nemo-loop-engine"
    namespace = "nemo-jobs"
  }
  role_ref {
    api_group = "rbac.authorization.k8s.io"
    kind      = "Role"
    name      = "nemo-loop-engine"
  }
  subject {
    kind      = "ServiceAccount"
    name      = "nemo-loop-engine"
    namespace = "nemo-system"
  }
}

resource "kubernetes_role" "api_server_jobs" {
  depends_on = [kubernetes_namespace.jobs]

  metadata {
    name      = "nemo-api-server"
    namespace = "nemo-jobs"
  }
  rule {
    api_groups = [""]
    resources  = ["secrets"]
    verbs      = ["create", "update", "get"]
  }
}

resource "kubernetes_role_binding" "api_server_jobs" {
  depends_on = [kubernetes_role.api_server_jobs, kubernetes_service_account.api_server]

  metadata {
    name      = "nemo-api-server"
    namespace = "nemo-jobs"
  }
  role_ref {
    api_group = "rbac.authorization.k8s.io"
    kind      = "Role"
    name      = "nemo-api-server"
  }
  subject {
    kind      = "ServiceAccount"
    name      = "nemo-api-server"
    namespace = "nemo-system"
  }
}

# --- Networking: TLS (conditional on domain) ---

resource "helm_release" "cert_manager" {
  count      = local.has_domain ? 1 : 0
  depends_on = [null_resource.kubeconfig]

  name             = "cert-manager"
  repository       = "https://charts.jetstack.io"
  chart            = "cert-manager"
  version          = var.cert_manager_version
  namespace        = "cert-manager"
  create_namespace = true

  set {
    name  = "installCRDs"
    value = "true"
  }
}

resource "kubernetes_manifest" "cluster_issuer" {
  count      = local.has_domain ? 1 : 0
  depends_on = [helm_release.cert_manager]

  manifest = {
    apiVersion = "cert-manager.io/v1"
    kind       = "ClusterIssuer"
    metadata   = { name = "letsencrypt-prod" }
    spec = {
      acme = {
        server              = "https://acme-v2.api.letsencrypt.org/directory"
        email               = var.acme_email
        privateKeySecretRef = { name = "letsencrypt-prod-key" }
        solvers = [{
          http01 = { ingress = { class = "traefik" } }
        }]
      }
    }
  }
}

resource "kubernetes_manifest" "redirect_https" {
  count      = local.has_domain ? 1 : 0
  depends_on = [kubernetes_namespace.system]

  manifest = {
    apiVersion = "traefik.io/v1alpha1"
    kind       = "Middleware"
    metadata = {
      name      = "redirect-https"
      namespace = "nemo-system"
    }
    spec = {
      redirectScheme = {
        scheme    = "https"
        permanent = true
      }
    }
  }
}

# HTTP entrypoint with domain: redirect to HTTPS (except ACME challenges)
resource "kubernetes_manifest" "api_ingress_http" {
  count      = local.has_domain ? 1 : 0
  depends_on = [kubernetes_manifest.redirect_https, kubernetes_namespace.system]

  manifest = {
    apiVersion = "traefik.io/v1alpha1"
    kind       = "IngressRoute"
    metadata = {
      name      = "nemo-api-http"
      namespace = "nemo-system"
    }
    spec = {
      entryPoints = ["web"]
      routes = [
        {
          match    = "Host(`${var.domain}`) && PathPrefix(`/.well-known/acme-challenge/`)"
          kind     = "Rule"
          priority = 100
          services = [{ name = "nemo-api-server", port = 8080 }]
        },
        {
          match = "Host(`${var.domain}`)"
          kind  = "Rule"
          middlewares = [{
            name      = "redirect-https"
            namespace = "nemo-system"
          }]
          services = [{ name = "nemo-api-server", port = 8080 }]
        },
      ]
    }
  }
}

# HTTPS entrypoint with domain
resource "kubernetes_manifest" "api_ingress" {
  count      = local.has_domain ? 1 : 0
  depends_on = [kubernetes_manifest.cluster_issuer, kubernetes_namespace.system]

  manifest = {
    apiVersion = "traefik.io/v1alpha1"
    kind       = "IngressRoute"
    metadata = {
      name      = "nemo-api"
      namespace = "nemo-system"
    }
    spec = {
      entryPoints = ["websecure"]
      routes = [{
        match    = "Host(`${var.domain}`) && !Path(`/health`)"
        kind     = "Rule"
        services = [{ name = "nemo-api-server", port = 8080 }]
      }]
      tls = { secretName = "nemo-tls" }
    }
  }
}

resource "kubernetes_manifest" "api_certificate" {
  count      = local.has_domain ? 1 : 0
  depends_on = [kubernetes_manifest.cluster_issuer, kubernetes_namespace.system]

  manifest = {
    apiVersion = "cert-manager.io/v1"
    kind       = "Certificate"
    metadata = {
      name      = "nemo-tls"
      namespace = "nemo-system"
    }
    spec = {
      secretName = "nemo-tls"
      issuerRef = {
        name = "letsencrypt-prod"
        kind = "ClusterIssuer"
      }
      dnsNames = [var.domain]
    }
  }
}

# IP-only mode: expose API server directly on port 8080 via LoadBalancer (k3s Klipper LB)
resource "kubernetes_service" "api_server_lb" {
  count      = local.has_domain ? 0 : 1
  depends_on = [kubernetes_namespace.system]

  metadata {
    name      = "nemo-api-server-lb"
    namespace = "nemo-system"
  }
  spec {
    type     = "LoadBalancer"
    selector = { app = "nemo-api-server" }
    port {
      port        = 8080
      target_port = 8080
    }
  }
}
