# Kubernetes resources applied via SSH+kubectl.
# No kubernetes/helm provider needed — avoids the chicken-and-egg problem
# where providers initialize at plan time before the kubeconfig exists.

# --- Locals: YAML manifests ---

locals {
  # Foundation: namespaces, PVs, PVCs
  foundation_yaml = <<-YAML
apiVersion: v1
kind: Namespace
metadata:
  name: nautiloop-system
  labels:
    app: nautiloop
---
apiVersion: v1
kind: Namespace
metadata:
  name: nautiloop-jobs
  labels:
    app: nautiloop
---
apiVersion: v1
kind: PersistentVolume
metadata:
  name: nautiloop-bare-repo-system
spec:
  capacity:
    storage: 100Gi
  accessModes: ["ReadWriteMany"]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: manual
  hostPath:
    path: /data/nautiloop-bare-repo
    type: DirectoryOrCreate
---
apiVersion: v1
kind: PersistentVolume
metadata:
  name: nautiloop-bare-repo-jobs
spec:
  capacity:
    storage: 100Gi
  accessModes: ["ReadWriteMany"]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: manual
  hostPath:
    path: /data/nautiloop-bare-repo
    type: DirectoryOrCreate
---
apiVersion: v1
kind: PersistentVolume
metadata:
  name: nautiloop-postgres-data
spec:
  capacity:
    storage: ${var.postgres_volume_size}Gi
  accessModes: ["ReadWriteOnce"]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: manual
  hostPath:
    path: /data/nautiloop-postgres
    type: DirectoryOrCreate
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: nautiloop-bare-repo
  namespace: nautiloop-system
spec:
  accessModes: ["ReadWriteMany"]
  storageClassName: manual
  volumeName: nautiloop-bare-repo-system
  resources:
    requests:
      storage: 100Gi
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: nautiloop-bare-repo
  namespace: nautiloop-jobs
spec:
  accessModes: ["ReadWriteMany"]
  storageClassName: manual
  volumeName: nautiloop-bare-repo-jobs
  resources:
    requests:
      storage: 100Gi
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: nautiloop-sessions
  namespace: nautiloop-jobs
spec:
  accessModes: ["ReadWriteOnce"]
  resources:
    requests:
      storage: 10Gi
---
# Shared cache volume for all caching tools (sccache, npm, pip, etc.).
# Mounted at /cache on implement/revise pods. Env vars in [cache.env]
# in nemo.toml tell each tool where its subdirectory lives.
# Single-node self-hosted clusters use RWO; multi-node needs RWX
# (or tool-native remote backends like sccache S3).
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: nautiloop-cache
  namespace: nautiloop-jobs
spec:
  accessModes: ["ReadWriteOnce"]
  resources:
    requests:
      storage: ${coalesce(var.cargo_cache_volume_size, var.cache_volume_size)}Gi
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: nautiloop-postgres-data
  namespace: nautiloop-system
spec:
  accessModes: ["ReadWriteOnce"]
  storageClassName: manual
  volumeName: nautiloop-postgres-data
  resources:
    requests:
      storage: ${var.postgres_volume_size}Gi
YAML

  # Secrets (using data: with base64-encoded values for safe transport)
  secrets_yaml = <<-YAML
apiVersion: v1
kind: Secret
metadata:
  name: nautiloop-postgres-credentials
  namespace: nautiloop-system
data:
  password: ${base64encode(local.postgres_password)}
  DATABASE_URL: ${base64encode("postgres://nautiloop:${local.postgres_password}@nautiloop-postgres:5432/nautiloop")}
---
apiVersion: v1
kind: Secret
metadata:
  name: nautiloop-api-key
  namespace: nautiloop-system
data:
  NAUTILOOP_API_KEY: ${base64encode(random_password.api_key.result)}
---
apiVersion: v1
kind: Secret
metadata:
  name: nautiloop-git-host-token
  namespace: nautiloop-system
data:
  GIT_HOST_TOKEN: ${base64encode(var.git_host_token)}
---
apiVersion: v1
kind: Secret
metadata:
  name: nautiloop-repo-ssh-key
  namespace: nautiloop-system
data:
  id_ed25519: ${base64encode(local.deploy_private_key)}
YAML

  # Judge credentials — prefers Claude OAuth bundle (Anthropic subscription),
  # falls back to raw Anthropic API key. Secret shape matches nautiloop-creds-<engineer>
  # so the auth-sidecar reads /secrets/model-credentials/{claude,anthropic} as-is.
  # Precedence (higher wins):
  #   1. judge_claude_credentials — JSON bundle from `nemo auth --claude` / ~/.claude/.credentials.json
  #   2. judge_anthropic_key — raw Anthropic API key (legacy fallback)
  _judge_has_claude    = var.judge_claude_credentials != null && var.judge_claude_credentials != ""
  _judge_has_anthropic = var.judge_anthropic_key != null && var.judge_anthropic_key != ""
  _judge_creds_data = (
    local._judge_has_claude ? "  claude: ${base64encode(coalesce(var.judge_claude_credentials, ""))}" :
    local._judge_has_anthropic ? "  anthropic: ${base64encode(coalesce(var.judge_anthropic_key, ""))}" :
    ""
  )
  _judge_creds_yaml_template = <<-YAML
apiVersion: v1
kind: Secret
metadata:
  name: nautiloop-judge-creds
  namespace: nautiloop-system
data:
${local._judge_creds_data}
YAML
  judge_creds_yaml           = (local._judge_has_claude || local._judge_has_anthropic) ? local._judge_creds_yaml_template : ""

  # Registry creds (only rendered when image_pull_secret provided)
  _dockerconfigjson_b64 = var.image_pull_secret_dockerconfigjson != null ? base64encode(var.image_pull_secret_dockerconfigjson) : ""

  registry_creds_yaml = <<-YAML
apiVersion: v1
kind: Secret
metadata:
  name: nautiloop-registry-creds
  namespace: nautiloop-jobs
type: kubernetes.io/dockerconfigjson
data:
  .dockerconfigjson: ${local._dockerconfigjson_b64}
---
apiVersion: v1
kind: Secret
metadata:
  name: nautiloop-registry-creds
  namespace: nautiloop-system
type: kubernetes.io/dockerconfigjson
data:
  .dockerconfigjson: ${local._dockerconfigjson_b64}
YAML

  # ConfigMaps
  config_yaml = <<-YAML
apiVersion: v1
kind: ConfigMap
metadata:
  name: nautiloop-cluster-config
  namespace: nautiloop-system
data:
  git_repo_url: "${var.git_repo_url}"
  domain: "${local.has_domain ? var.domain : var.server_ip}"
${var.ssh_known_hosts != "" ? <<-YAML
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: nautiloop-ssh-known-hosts
  namespace: nautiloop-system
data:
  known_hosts: |
    ${indent(4, var.ssh_known_hosts)}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: nautiloop-ssh-known-hosts
  namespace: nautiloop-jobs
data:
  known_hosts: |
    ${indent(4, var.ssh_known_hosts)}
YAML
: ""}
YAML

# RBAC: service accounts, roles, role bindings
rbac_yaml = <<-YAML
apiVersion: v1
kind: ServiceAccount
metadata:
  name: nautiloop-loop-engine
  namespace: nautiloop-system
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: nautiloop-api-server
  namespace: nautiloop-system
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: nautiloop-loop-engine
  namespace: nautiloop-jobs
rules:
  - apiGroups: ["batch"]
    resources: ["jobs"]
    verbs: ["create", "delete", "list", "watch", "get"]
  - apiGroups: [""]
    resources: ["pods"]
    verbs: ["list", "get"]
  - apiGroups: [""]
    resources: ["pods/log"]
    verbs: ["get"]
  - apiGroups: [""]
    resources: ["secrets"]
    verbs: ["create", "update", "get"]
  - apiGroups: [""]
    resources: ["configmaps"]
    verbs: ["create", "update", "get"]
  - apiGroups: [""]
    resources: ["persistentvolumeclaims"]
    verbs: ["get", "list"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: nautiloop-loop-engine
  namespace: nautiloop-jobs
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: nautiloop-loop-engine
subjects:
  - kind: ServiceAccount
    name: nautiloop-loop-engine
    namespace: nautiloop-system
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: nautiloop-api-server
  namespace: nautiloop-jobs
rules:
  - apiGroups: [""]
    resources: ["secrets"]
    verbs: ["create", "update", "get"]
  # Matches dev/k8s/02-rbac.yaml. The dashboard log-stream endpoints
  # (/logs, /pod-logs, /ps) and /cache disk-usage endpoint all need to
  # list pods, read their logs, and exec into running agent pods in
  # this namespace. Without these rules every tail returns 403. See
  # control-plane/src/api/handlers.rs:538 and api/cache.rs:142.
  - apiGroups: [""]
    resources: ["pods"]
    verbs: ["list", "get"]
  - apiGroups: [""]
    resources: ["pods/log"]
    verbs: ["get"]
  - apiGroups: [""]
    resources: ["pods/exec"]
    verbs: ["get", "create"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: nautiloop-api-server
  namespace: nautiloop-jobs
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: nautiloop-api-server
subjects:
  - kind: ServiceAccount
    name: nautiloop-api-server
    namespace: nautiloop-system
YAML

# Networking: TLS mode (domain set) — cert-manager resources + IngressRoutes
# Always rendered; only applied when has_domain is true (via count on null_resource)
_acme_email         = var.acme_email != null ? var.acme_email : ""
_domain             = var.domain != null ? var.domain : ""
networking_tls_yaml = <<-YAML
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: letsencrypt-prod
spec:
  acme:
    server: https://acme-v2.api.letsencrypt.org/directory
    email: ${local._acme_email}
    privateKeySecretRef:
      name: letsencrypt-prod-key
    solvers:
      - http01:
          ingress:
            class: traefik
---
apiVersion: traefik.io/v1alpha1
kind: Middleware
metadata:
  name: redirect-https
  namespace: nautiloop-system
spec:
  redirectScheme:
    scheme: https
    permanent: true
---
apiVersion: traefik.io/v1alpha1
kind: IngressRoute
metadata:
  name: nautiloop-api-http
  namespace: nautiloop-system
spec:
  entryPoints: ["web"]
  routes:
    - match: "Host(`${local._domain}`) && PathPrefix(`/.well-known/acme-challenge/`)"
      kind: Rule
      priority: 100
      services:
        - name: nautiloop-api-server
          port: 8080
    - match: "Host(`${local._domain}`)"
      kind: Rule
      middlewares:
        - name: redirect-https
          namespace: nautiloop-system
      services:
        - name: nautiloop-api-server
          port: 8080
---
apiVersion: traefik.io/v1alpha1
kind: IngressRoute
metadata:
  name: nautiloop-api
  namespace: nautiloop-system
spec:
  entryPoints: ["websecure"]
  routes:
    - match: "Host(`${local._domain}`) && !Path(`/health`)"
      kind: Rule
      services:
        - name: nautiloop-api-server
          port: 8080
  tls:
    secretName: nautiloop-tls
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: nautiloop-tls
  namespace: nautiloop-system
spec:
  secretName: nautiloop-tls
  issuerRef:
    name: letsencrypt-prod
    kind: ClusterIssuer
  dnsNames:
    - ${local._domain}
YAML

# Networking: IP-only mode (no domain) — LoadBalancer service
networking_ip_yaml = <<-YAML
apiVersion: v1
kind: Service
metadata:
  name: nautiloop-api-server-lb
  namespace: nautiloop-system
spec:
  type: LoadBalancer
  selector:
    app: nautiloop-api-server
  ports:
    - port: 8080
      targetPort: 8080
YAML
}

# --- Stage 1: Foundation (namespaces, PVs, PVCs) ---

resource "null_resource" "k8s_foundation" {
  depends_on = [null_resource.k3s_install]

  # Only trigger replacement on the manifest content, not connection details.
  # Changing server_ip/ssh_key should NOT tear down the cluster foundation.
  triggers = {
    manifest_hash = sha256(local.foundation_yaml)
    server_ip     = var.server_ip
  }

  connection {
    type        = "ssh"
    host        = var.server_ip
    user        = var.ssh_user
    private_key = var.ssh_private_key
  }

  provisioner "remote-exec" {
    inline = [
      "echo '${base64encode(local.foundation_yaml)}' | base64 -d | kubectl apply -f -",
    ]
  }
}

# NOTE: No destroy provisioner here. On `terraform destroy`:
# - Cloud-provisioned servers (Hetzner, etc.) are destroyed entirely
# - Existing servers: run `k3s-uninstall.sh` to clean up k3s + all resources

# --- Stage 2: Secrets ---

resource "null_resource" "k8s_secrets" {
  depends_on = [null_resource.k8s_foundation]

  triggers = {
    secrets_hash = sha256(local.secrets_yaml)
    registry     = var.image_pull_secret_dockerconfigjson != null ? "present" : "absent"
    server_ip    = var.server_ip
  }

  connection {
    type        = "ssh"
    host        = var.server_ip
    user        = var.ssh_user
    private_key = var.ssh_private_key
  }

  # NOTE: Secrets pass through a remote-exec inline command as base64.
  # Terraform's SSH provisioner writes inline commands to a temp script that
  # is removed after execution. On this module's single-tenant k3s node,
  # root (the SSH user) already has full access to all k8s secrets via kubectl.
  provisioner "remote-exec" {
    inline = [
      "echo '${base64encode(local.secrets_yaml)}' | base64 -d | kubectl apply -f -",
    ]
  }
}

# Judge credentials (conditional — only when judge_anthropic_key provided)
resource "null_resource" "k8s_judge_creds" {
  count      = var.judge_anthropic_key != null ? 1 : 0
  depends_on = [null_resource.k8s_foundation]

  triggers = {
    key_hash  = sha256(var.judge_anthropic_key)
    server_ip = var.server_ip
  }

  connection {
    type        = "ssh"
    host        = var.server_ip
    user        = var.ssh_user
    private_key = var.ssh_private_key
  }

  provisioner "remote-exec" {
    inline = [
      "echo '${base64encode(local.judge_creds_yaml)}' | base64 -d | kubectl apply -f -",
    ]
  }
}

# Registry credentials (conditional — only when image_pull_secret provided)
resource "null_resource" "k8s_registry_creds" {
  count      = var.image_pull_secret_dockerconfigjson != null ? 1 : 0
  depends_on = [null_resource.k8s_foundation]

  triggers = {
    creds_hash = sha256(local.registry_creds_yaml)
    server_ip  = var.server_ip
  }

  connection {
    type        = "ssh"
    host        = var.server_ip
    user        = var.ssh_user
    private_key = var.ssh_private_key
  }

  provisioner "remote-exec" {
    inline = [
      "echo '${base64encode(local.registry_creds_yaml)}' | base64 -d | kubectl apply -f -",
    ]
  }
}

# --- Stage 3: ConfigMaps + RBAC ---

resource "null_resource" "k8s_config" {
  depends_on = [null_resource.k8s_foundation]

  triggers = {
    config_hash = sha256(local.config_yaml)
    rbac_hash   = sha256(local.rbac_yaml)
    server_ip   = var.server_ip
  }

  connection {
    type        = "ssh"
    host        = var.server_ip
    user        = var.ssh_user
    private_key = var.ssh_private_key
  }

  provisioner "remote-exec" {
    inline = [
      "echo '${base64encode(local.config_yaml)}' | base64 -d | kubectl apply -f -",
      "echo '${base64encode(local.rbac_yaml)}' | base64 -d | kubectl apply -f -",
    ]
  }
}

# Fallback ssh-keyscan if ssh_known_hosts not provided.
# Uses local kubectl with the generated kubeconfig, so must wait for it.
resource "null_resource" "ssh_keyscan" {
  count = var.ssh_known_hosts == "" ? 1 : 0

  depends_on = [null_resource.k8s_config, null_resource.kubeconfig]

  triggers = {
    git_repo_url = var.git_repo_url
    config_hash  = sha256(local.config_yaml)
  }

  provisioner "local-exec" {
    command = <<-EOT
      GITHOST=$(echo "$GIT_REPO_URL" | sed -E 's/.*@([^:]+):.*/\1/' | sed -E 's|https?://([^/]+).*|\1|')
      KNOWN_HOSTS=$(ssh-keyscan "$GITHOST" 2>/dev/null)
      if [ -z "$KNOWN_HOSTS" ]; then
        echo "ERROR: ssh-keyscan returned empty for $GITHOST" >&2
        exit 1
      fi
      for NS in nautiloop-system nautiloop-jobs; do
        kubectl --kubeconfig ${local.kubeconfig_path} -n "$NS" \
          create configmap nautiloop-ssh-known-hosts \
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

# --- Stage 4: cert-manager (conditional on domain) ---

resource "null_resource" "k8s_cert_manager" {
  count      = local.has_domain ? 1 : 0
  depends_on = [null_resource.k8s_foundation]

  triggers = {
    cert_manager_version = var.cert_manager_version
    server_ip            = var.server_ip
  }

  connection {
    type        = "ssh"
    host        = var.server_ip
    user        = var.ssh_user
    private_key = var.ssh_private_key
  }

  provisioner "remote-exec" {
    inline = [
      # Install helm if not present (k3s doesn't include it)
      "command -v helm >/dev/null 2>&1 || curl -fsSL https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash",
      "helm repo add jetstack https://charts.jetstack.io 2>/dev/null || helm repo update jetstack",
      "helm upgrade --install cert-manager jetstack/cert-manager --namespace cert-manager --create-namespace --version '${var.cert_manager_version}' --set installCRDs=true --wait --timeout 5m",
    ]
  }

  # No destroy provisioner — k3s-uninstall.sh handles full cleanup.
  # Terraform destroy provisioners can't safely reference variables.
}

# --- Stage 5a: Networking — TLS mode (domain set) ---

resource "null_resource" "k8s_networking_tls" {
  count = local.has_domain ? 1 : 0

  depends_on = [
    null_resource.k8s_foundation,
    null_resource.k8s_cert_manager,
  ]

  triggers = {
    manifest_hash = sha256(local.networking_tls_yaml)
    server_ip     = var.server_ip
  }

  connection {
    type        = "ssh"
    host        = var.server_ip
    user        = var.ssh_user
    private_key = var.ssh_private_key
  }

  provisioner "remote-exec" {
    inline = [
      "echo '${base64encode(local.networking_tls_yaml)}' | base64 -d | kubectl apply -f -",
      # Clean up IP-only LoadBalancer if it exists from a previous IP-only deployment
      "kubectl -n nautiloop-system delete svc nautiloop-api-server-lb --ignore-not-found",
    ]
  }
}

# --- Stage 5b: Networking — IP-only mode (no domain) ---

resource "null_resource" "k8s_networking_ip" {
  count = local.has_domain ? 0 : 1

  depends_on = [null_resource.k8s_foundation]

  triggers = {
    manifest_hash = sha256(local.networking_ip_yaml)
    server_ip     = var.server_ip
  }

  connection {
    type        = "ssh"
    host        = var.server_ip
    user        = var.ssh_user
    private_key = var.ssh_private_key
  }

  provisioner "remote-exec" {
    inline = [
      "echo '${base64encode(local.networking_ip_yaml)}' | base64 -d | kubectl apply -f -",
      # Clean up TLS resources if they exist from a previous domain deployment
      "kubectl -n nautiloop-system delete ingressroute nautiloop-api nautiloop-api-http --ignore-not-found 2>/dev/null || true",
      "kubectl -n nautiloop-system delete middleware redirect-https --ignore-not-found 2>/dev/null || true",
      "kubectl -n nautiloop-system delete certificate nautiloop-tls --ignore-not-found 2>/dev/null || true",
      "kubectl delete clusterissuer letsencrypt-prod --ignore-not-found 2>/dev/null || true",
      # Uninstall cert-manager helm release if it was previously installed
      "command -v helm >/dev/null 2>&1 && helm uninstall cert-manager --namespace cert-manager 2>/dev/null || true",
      "kubectl delete namespace cert-manager --ignore-not-found 2>/dev/null || true",
    ]
  }
}
