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
  name: nemo-system
  labels:
    app: nemo
---
apiVersion: v1
kind: Namespace
metadata:
  name: nemo-jobs
  labels:
    app: nemo
---
apiVersion: v1
kind: PersistentVolume
metadata:
  name: nemo-bare-repo-system
spec:
  capacity:
    storage: 100Gi
  accessModes: ["ReadWriteMany"]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: manual
  hostPath:
    path: /data/nemo-bare-repo
    type: DirectoryOrCreate
---
apiVersion: v1
kind: PersistentVolume
metadata:
  name: nemo-bare-repo-jobs
spec:
  capacity:
    storage: 100Gi
  accessModes: ["ReadWriteMany"]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: manual
  hostPath:
    path: /data/nemo-bare-repo
    type: DirectoryOrCreate
---
apiVersion: v1
kind: PersistentVolume
metadata:
  name: nemo-postgres-data
spec:
  capacity:
    storage: ${var.postgres_volume_size}Gi
  accessModes: ["ReadWriteOnce"]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: manual
  hostPath:
    path: /data/nemo-postgres
    type: DirectoryOrCreate
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: nemo-bare-repo
  namespace: nemo-system
spec:
  accessModes: ["ReadWriteMany"]
  storageClassName: manual
  volumeName: nemo-bare-repo-system
  resources:
    requests:
      storage: 100Gi
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: nemo-bare-repo
  namespace: nemo-jobs
spec:
  accessModes: ["ReadWriteMany"]
  storageClassName: manual
  volumeName: nemo-bare-repo-jobs
  resources:
    requests:
      storage: 100Gi
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: nemo-sessions
  namespace: nemo-jobs
spec:
  accessModes: ["ReadWriteOnce"]
  resources:
    requests:
      storage: 10Gi
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: nemo-postgres-data
  namespace: nemo-system
spec:
  accessModes: ["ReadWriteOnce"]
  storageClassName: manual
  volumeName: nemo-postgres-data
  resources:
    requests:
      storage: ${var.postgres_volume_size}Gi
YAML

  # Secrets (using data: with base64-encoded values for safe transport)
  secrets_yaml = <<-YAML
apiVersion: v1
kind: Secret
metadata:
  name: nemo-postgres-credentials
  namespace: nemo-system
data:
  password: ${base64encode(local.postgres_password)}
  DATABASE_URL: ${base64encode("postgres://nemo:${local.postgres_password}@nemo-postgres:5432/nemo")}
---
apiVersion: v1
kind: Secret
metadata:
  name: nemo-api-key
  namespace: nemo-system
data:
  NEMO_API_KEY: ${base64encode(random_password.api_key.result)}
---
apiVersion: v1
kind: Secret
metadata:
  name: nemo-git-host-token
  namespace: nemo-system
data:
  GIT_HOST_TOKEN: ${base64encode(var.git_host_token)}
---
apiVersion: v1
kind: Secret
metadata:
  name: nemo-repo-ssh-key
  namespace: nemo-system
data:
  id_ed25519: ${base64encode(local.deploy_private_key)}
YAML

  # Registry creds (only rendered when image_pull_secret provided)
  _dockerconfigjson_b64 = var.image_pull_secret_dockerconfigjson != null ? base64encode(var.image_pull_secret_dockerconfigjson) : ""

  registry_creds_yaml = <<-YAML
apiVersion: v1
kind: Secret
metadata:
  name: nemo-registry-creds
  namespace: nemo-jobs
type: kubernetes.io/dockerconfigjson
data:
  .dockerconfigjson: ${local._dockerconfigjson_b64}
---
apiVersion: v1
kind: Secret
metadata:
  name: nemo-registry-creds
  namespace: nemo-system
type: kubernetes.io/dockerconfigjson
data:
  .dockerconfigjson: ${local._dockerconfigjson_b64}
YAML

  # ConfigMaps
  config_yaml = <<-YAML
apiVersion: v1
kind: ConfigMap
metadata:
  name: nemo-cluster-config
  namespace: nemo-system
data:
  git_repo_url: "${var.git_repo_url}"
  domain: "${local.has_domain ? var.domain : var.server_ip}"
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: nemo-ssh-known-hosts
  namespace: nemo-system
data:
  known_hosts: |
    ${indent(4, var.ssh_known_hosts)}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: nemo-ssh-known-hosts
  namespace: nemo-jobs
data:
  known_hosts: |
    ${indent(4, var.ssh_known_hosts)}
YAML

  # RBAC: service accounts, roles, role bindings
  rbac_yaml = <<-YAML
apiVersion: v1
kind: ServiceAccount
metadata:
  name: nemo-loop-engine
  namespace: nemo-system
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: nemo-api-server
  namespace: nemo-system
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: nemo-loop-engine
  namespace: nemo-jobs
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
  name: nemo-loop-engine
  namespace: nemo-jobs
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: nemo-loop-engine
subjects:
  - kind: ServiceAccount
    name: nemo-loop-engine
    namespace: nemo-system
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: nemo-api-server
  namespace: nemo-jobs
rules:
  - apiGroups: [""]
    resources: ["secrets"]
    verbs: ["create", "update", "get"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: nemo-api-server
  namespace: nemo-jobs
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: nemo-api-server
subjects:
  - kind: ServiceAccount
    name: nemo-api-server
    namespace: nemo-system
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
  namespace: nemo-system
spec:
  redirectScheme:
    scheme: https
    permanent: true
---
apiVersion: traefik.io/v1alpha1
kind: IngressRoute
metadata:
  name: nemo-api-http
  namespace: nemo-system
spec:
  entryPoints: ["web"]
  routes:
    - match: "Host(`${local._domain}`) && PathPrefix(`/.well-known/acme-challenge/`)"
      kind: Rule
      priority: 100
      services:
        - name: nemo-api-server
          port: 8080
    - match: "Host(`${local._domain}`)"
      kind: Rule
      middlewares:
        - name: redirect-https
          namespace: nemo-system
      services:
        - name: nemo-api-server
          port: 8080
---
apiVersion: traefik.io/v1alpha1
kind: IngressRoute
metadata:
  name: nemo-api
  namespace: nemo-system
spec:
  entryPoints: ["websecure"]
  routes:
    - match: "Host(`${local._domain}`) && !Path(`/health`)"
      kind: Rule
      services:
        - name: nemo-api-server
          port: 8080
  tls:
    secretName: nemo-tls
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: nemo-tls
  namespace: nemo-system
spec:
  secretName: nemo-tls
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
  name: nemo-api-server-lb
  namespace: nemo-system
spec:
  type: LoadBalancer
  selector:
    app: nemo-api-server
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
      "kubectl -n nemo-system delete svc nemo-api-server-lb --ignore-not-found",
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
      "kubectl -n nemo-system delete ingressroute nemo-api nemo-api-http --ignore-not-found 2>/dev/null || true",
      "kubectl -n nemo-system delete middleware redirect-https --ignore-not-found 2>/dev/null || true",
      "kubectl -n nemo-system delete certificate nemo-tls --ignore-not-found 2>/dev/null || true",
      "kubectl delete clusterissuer letsencrypt-prod --ignore-not-found 2>/dev/null || true",
      # Uninstall cert-manager helm release if it was previously installed
      "command -v helm >/dev/null 2>&1 && helm uninstall cert-manager --namespace cert-manager 2>/dev/null || true",
      "kubectl delete namespace cert-manager --ignore-not-found 2>/dev/null || true",
    ]
  }
}
