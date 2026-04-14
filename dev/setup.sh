#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# ── Prerequisites ──────────────────────────────────────────────────────────────

check_tool() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "WARN: '$1' not found on PATH. ${2:-Install it before continuing.}"
    fi
}

check_tool k3d    "See https://k3d.io/"
check_tool kubectl "See https://kubernetes.io/docs/tasks/tools/"
check_tool docker  "See https://docs.docker.com/get-docker/"
check_tool cargo   "See https://www.rust-lang.org/tools/install"
check_tool nemo    "Run: cargo install --path ${REPO_ROOT}/cli"

# ── Required env vars ─────────────────────────────────────────────────────────

NAUTILOOP_GIT_REPO_URL="${NAUTILOOP_GIT_REPO_URL:-}"
NAUTILOOP_GITHUB_TOKEN="${NAUTILOOP_GITHUB_TOKEN:-}"
NAUTILOOP_ENGINEER="${NAUTILOOP_ENGINEER:-dev}"
NAUTILOOP_SSH_PRIVATE_KEY_PATH="${NAUTILOOP_SSH_PRIVATE_KEY_PATH:-${HOME}/.ssh/id_ed25519}"
NAUTILOOP_OPENAI_KEY="${NAUTILOOP_OPENAI_KEY:-}"
NAUTILOOP_ANTHROPIC_KEY="${NAUTILOOP_ANTHROPIC_KEY:-}"

MISSING=false
if [ -z "$NAUTILOOP_GIT_REPO_URL" ]; then
    echo "ERROR: NAUTILOOP_GIT_REPO_URL is not set (e.g. git@github.com:org/repo.git)"
    MISSING=true
fi
if [ -z "$NAUTILOOP_GITHUB_TOKEN" ]; then
    echo "ERROR: NAUTILOOP_GITHUB_TOKEN is not set (GitHub PAT for PR operations)"
    MISSING=true
fi
if [ ! -f "$NAUTILOOP_SSH_PRIVATE_KEY_PATH" ]; then
    echo "ERROR: SSH private key not found at ${NAUTILOOP_SSH_PRIVATE_KEY_PATH}"
    echo "       Set NAUTILOOP_SSH_PRIVATE_KEY_PATH to the path of your SSH key."
    MISSING=true
fi
if "$MISSING"; then
    exit 1
fi

if [ -z "$NAUTILOOP_OPENAI_KEY" ] && [ -z "$NAUTILOOP_ANTHROPIC_KEY" ]; then
    echo "WARN: Neither NAUTILOOP_OPENAI_KEY nor NAUTILOOP_ANTHROPIC_KEY is set."
    echo "      Agent jobs will fail at model calls. Set at least one."
fi

# ── k3d registry ──────────────────────────────────────────────────────────────

if k3d registry list 2>/dev/null | grep -q "nautiloop-registry"; then
    echo "==> Registry k3d-nautiloop-registry already exists, skipping."
else
    echo "==> Creating k3d registry nautiloop-registry on port 5001..."
    k3d registry create nautiloop-registry --port 5001
fi

# ── k3d cluster ───────────────────────────────────────────────────────────────

if k3d cluster list 2>/dev/null | grep -q "nautiloop-dev"; then
    echo "==> Cluster nautiloop-dev already exists, skipping creation."
else
    echo "==> Creating k3d cluster nautiloop-dev..."
    k3d cluster create nautiloop-dev \
        --registry-use k3d-nautiloop-registry:5001 \
        --k3s-arg "--disable=traefik@server:0" \
        --agents 0 \
        -p "18080:80@loadbalancer"
fi

# Make sure kubectl context is set to the dev cluster
kubectl config use-context k3d-nautiloop-dev

# ── Build and push images ─────────────────────────────────────────────────────

echo "==> Building and pushing images..."
"${SCRIPT_DIR}/build.sh"

# ── Apply manifests ───────────────────────────────────────────────────────────

echo "==> Applying Kubernetes manifests..."
kubectl apply -f "${SCRIPT_DIR}/k8s/"

# ── Secrets ───────────────────────────────────────────────────────────────────

echo "==> Creating secrets..."

# Postgres credentials — read existing secret if present (idempotent on re-runs).
EXISTING_PG_PASSWORD="$(kubectl -n nautiloop-system get secret nautiloop-postgres-credentials \
    -o jsonpath='{.data.password}' 2>/dev/null | base64 -d 2>/dev/null || true)"
if [ -n "$EXISTING_PG_PASSWORD" ]; then
    POSTGRES_PASSWORD="$EXISTING_PG_PASSWORD"
    echo "    Using existing Postgres password from secret."
else
    POSTGRES_PASSWORD="nautiloop-dev-$(openssl rand -hex 8)"
fi
kubectl -n nautiloop-system create secret generic nautiloop-postgres-credentials \
    --from-literal=password="${POSTGRES_PASSWORD}" \
    --from-literal=DATABASE_URL="postgres://nautiloop:${POSTGRES_PASSWORD}@nautiloop-postgres:5432/nautiloop" \
    --dry-run=client -o yaml | kubectl apply -f -

# API key — read existing secret if present (idempotent on re-runs).
EXISTING_API_KEY="$(kubectl -n nautiloop-system get secret nautiloop-api-key \
    -o jsonpath='{.data.NAUTILOOP_API_KEY}' 2>/dev/null | base64 -d 2>/dev/null || true)"
if [ -n "$EXISTING_API_KEY" ]; then
    NAUTILOOP_API_KEY="$EXISTING_API_KEY"
    echo "    Using existing API key from secret."
else
    NAUTILOOP_API_KEY="dev-api-key-$(openssl rand -hex 8)"
fi
kubectl -n nautiloop-system create secret generic nautiloop-api-key \
    --from-literal=NAUTILOOP_API_KEY="${NAUTILOOP_API_KEY}" \
    --dry-run=client -o yaml | kubectl apply -f -

# Git host token
kubectl -n nautiloop-system create secret generic nautiloop-git-host-token \
    --from-literal=GIT_HOST_TOKEN="${NAUTILOOP_GITHUB_TOKEN}" \
    --dry-run=client -o yaml | kubectl apply -f -

# SSH key for repo access
kubectl -n nautiloop-system create secret generic nautiloop-repo-ssh-key \
    --from-file=id_ed25519="${NAUTILOOP_SSH_PRIVATE_KEY_PATH}" \
    --dry-run=client -o yaml | kubectl apply -f -

# Engineer credentials secret (for agent jobs)
SAFE_ENGINEER="$(echo "${NAUTILOOP_ENGINEER}" | tr '[:upper:]' '[:lower:]' | tr '_' '-')"

SSH_KEY_B64="$(base64 < "${NAUTILOOP_SSH_PRIVATE_KEY_PATH}")"
CREDS_ARGS=(
    "--from-literal=ssh=${SSH_KEY_B64}"
)
if [ -n "$NAUTILOOP_ANTHROPIC_KEY" ]; then
    CREDS_ARGS+=("--from-literal=anthropic=${NAUTILOOP_ANTHROPIC_KEY}")
fi
if [ -n "$NAUTILOOP_OPENAI_KEY" ]; then
    CREDS_ARGS+=("--from-literal=openai=${NAUTILOOP_OPENAI_KEY}")
fi
kubectl -n nautiloop-jobs create secret generic "nautiloop-creds-${SAFE_ENGINEER}" \
    "${CREDS_ARGS[@]}" \
    --dry-run=client -o yaml | kubectl apply -f -

# ── Wait for Postgres ─────────────────────────────────────────────────────────

echo "==> Waiting for Postgres to be ready..."
kubectl rollout status deployment/nautiloop-postgres -n nautiloop-system --timeout=120s

# ── Update ConfigMap with Postgres password ───────────────────────────────────

echo "==> Updating nemo.toml ConfigMap..."
kubectl -n nautiloop-system create configmap nautiloop-config \
    --from-literal=nemo.toml="$(cat <<TOML
[cluster]
git_repo_url = "${NAUTILOOP_GIT_REPO_URL}"
agent_image = "k3d-nautiloop-registry:5001/nautiloop-agent-base:dev"
sidecar_image = "k3d-nautiloop-registry:5001/nautiloop-sidecar:dev"
skip_iptables = true
database_url = "postgres://nautiloop:${POSTGRES_PASSWORD}@nautiloop-postgres:5432/nautiloop"
TOML
)" \
    --dry-run=client -o yaml | kubectl apply -f -

# ── Repo init job ─────────────────────────────────────────────────────────────

echo "==> Running repo-init job..."

# Run ssh-keyscan to get known_hosts for the git host
GIT_HOST="$(echo "${NAUTILOOP_GIT_REPO_URL}" | sed -E 's/.*@([^:]+):.*/\1/' | sed -E 's|https?://([^/]+).*|\1|')"
KNOWN_HOSTS="$(ssh-keyscan "${GIT_HOST}" 2>/dev/null)"
if [ -z "$KNOWN_HOSTS" ]; then
    echo "WARN: ssh-keyscan returned empty for ${GIT_HOST}. Known-hosts will not be set."
fi

for NS in nautiloop-system nautiloop-jobs; do
    kubectl -n "${NS}" create configmap nautiloop-ssh-known-hosts \
        --from-literal=known_hosts="${KNOWN_HOSTS}" \
        --dry-run=client -o yaml | kubectl apply -f -
done

kubectl -n nautiloop-system delete job nautiloop-repo-init --ignore-not-found
kubectl -n nautiloop-system apply -f - <<EOF
apiVersion: batch/v1
kind: Job
metadata:
  name: nautiloop-repo-init
  namespace: nautiloop-system
spec:
  backoffLimit: 3
  template:
    spec:
      securityContext:
        runAsUser: 1000
        runAsGroup: 1000
        fsGroup: 1000
      containers:
        - name: repo-init
          image: alpine/git:latest
          command: ["/bin/sh", "-c"]
          args:
            - |
              set -e
              if [ ! -e /bare-repo/HEAD ]; then
                git init --bare /bare-repo
              fi
              git -C /bare-repo remote remove origin 2>/dev/null || true
              git -C /bare-repo remote add origin "\${GIT_REPO_URL}"
              mkdir -p "\${HOME}/.ssh"
              cp /secrets/ssh-key/id_ed25519 "\${HOME}/.ssh/id_ed25519"
              chmod 600 "\${HOME}/.ssh/id_ed25519"
              cp /secrets/ssh-known-hosts/known_hosts "\${HOME}/.ssh/known_hosts"
              git -C /bare-repo fetch --all || echo "WARN: git fetch failed (deploy key may not be configured yet)"
          env:
            - name: HOME
              value: /tmp
            - name: GIT_REPO_URL
              value: "${NAUTILOOP_GIT_REPO_URL}"
          volumeMounts:
            - name: bare-repo
              mountPath: /bare-repo
            - name: ssh-key
              mountPath: /secrets/ssh-key
              readOnly: true
            - name: ssh-known-hosts
              mountPath: /secrets/ssh-known-hosts
              readOnly: true
      volumes:
        - name: bare-repo
          persistentVolumeClaim:
            claimName: nautiloop-bare-repo
        - name: ssh-key
          secret:
            secretName: nautiloop-repo-ssh-key
            defaultMode: 0400
        - name: ssh-known-hosts
          configMap:
            name: nautiloop-ssh-known-hosts
      restartPolicy: OnFailure
EOF

kubectl -n nautiloop-system wait --for=condition=complete job/nautiloop-repo-init --timeout=300s || {
    echo "ERROR: repo-init job failed. Logs:"
    kubectl -n nautiloop-system logs -l job-name=nautiloop-repo-init
    exit 1
}

# ── Restart control plane to pick up updated secrets and config ───────────────

echo "==> Restarting control plane deployments..."
kubectl rollout restart deployment/nautiloop-api-server deployment/nautiloop-loop-engine \
    -n nautiloop-system

# ── Wait for control plane ────────────────────────────────────────────────────

echo "==> Waiting for control plane to be ready..."
kubectl rollout status deployment/nautiloop-api-server -n nautiloop-system --timeout=300s
kubectl rollout status deployment/nautiloop-loop-engine -n nautiloop-system --timeout=300s

# ── Configure nemo CLI ────────────────────────────────────────────────────────

echo "==> Configuring nemo CLI..."
nemo config --set server_url=http://localhost:18080
nemo config --set engineer="${NAUTILOOP_ENGINEER}"
nemo config --set api_key="${NAUTILOOP_API_KEY}"

# ── Done ──────────────────────────────────────────────────────────────────────

echo ""
echo "Nautiloop dev cluster is ready."
echo ""
echo "  API server:  http://localhost:18080"
echo "  Engineer:    ${NAUTILOOP_ENGINEER}"
echo "  API key:     ${NAUTILOOP_API_KEY}"
echo ""
echo "Run a smoke test:"
echo "  ./dev/smoke-test.sh"
echo ""
echo "Tear down when done:"
echo "  ./dev/teardown.sh"
