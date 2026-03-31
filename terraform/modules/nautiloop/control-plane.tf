# Control plane: nemo.toml config, repo-init job, API server, loop engine — applied via SSH+kubectl.

locals {
  nemo_toml = <<-TOML
[cluster]
git_repo_url = "${var.git_repo_url}"
agent_image = "${var.agent_base_image}"
sidecar_image = "${var.sidecar_image}"
${var.image_pull_secret_dockerconfigjson != null ? "image_pull_secret = \"nemo-registry-creds\"" : ""}
TOML

  config_checksum = sha256(local.nemo_toml)

  nemo_config_yaml = <<-YAML
apiVersion: v1
kind: ConfigMap
metadata:
  name: nemo-config
  namespace: nemo-system
data:
  nemo.toml: |
    ${indent(4, local.nemo_toml)}
YAML

  image_pull_secrets_snippet = var.image_pull_secret_dockerconfigjson != null ? "imagePullSecrets:\n        - name: nemo-registry-creds" : ""

  repo_init_yaml = <<-YAML
apiVersion: batch/v1
kind: Job
metadata:
  name: nemo-repo-init
  namespace: nemo-system
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
              git -C /bare-repo remote add origin "$GIT_REPO_URL"
              mkdir -p "$HOME/.ssh"
              cp /secrets/ssh-key/id_ed25519 "$HOME/.ssh/id_ed25519"
              chmod 600 "$HOME/.ssh/id_ed25519"
              cp /secrets/ssh-known-hosts/known_hosts "$HOME/.ssh/known_hosts"
              git -C /bare-repo fetch --all || echo "WARN: git fetch failed (deploy key may not be configured yet)"
          env:
            - name: HOME
              value: /tmp
            - name: GIT_REPO_URL
              valueFrom:
                configMapKeyRef:
                  name: nemo-cluster-config
                  key: git_repo_url
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
            claimName: nemo-bare-repo
        - name: ssh-key
          secret:
            secretName: nemo-repo-ssh-key
            defaultMode: 0400
        - name: ssh-known-hosts
          configMap:
            name: nemo-ssh-known-hosts
      restartPolicy: OnFailure
YAML

  api_server_yaml = <<-YAML
apiVersion: apps/v1
kind: Deployment
metadata:
  name: nemo-api-server
  namespace: nemo-system
  labels:
    app: nemo-api-server
spec:
  replicas: 1
  selector:
    matchLabels:
      app: nemo-api-server
  template:
    metadata:
      labels:
        app: nemo-api-server
      annotations:
        config-checksum: "${local.config_checksum}"
    spec:
      serviceAccountName: nemo-api-server
      ${local.image_pull_secrets_snippet}
      securityContext:
        fsGroup: 1000
      containers:
        - name: api-server
          image: ${var.control_plane_image}
          args: ["api-server"]
          ports:
            - containerPort: 8080
          env:
            - name: DATABASE_URL
              valueFrom:
                secretKeyRef:
                  name: nemo-postgres-credentials
                  key: DATABASE_URL
            - name: NEMO_API_KEY
              valueFrom:
                secretKeyRef:
                  name: nemo-api-key
                  key: NEMO_API_KEY
            - name: GIT_HOST_TOKEN
              valueFrom:
                secretKeyRef:
                  name: nemo-git-host-token
                  key: GIT_HOST_TOKEN
            - name: BARE_REPO_PATH
              value: /bare-repo
          resources:
            requests:
              cpu: 100m
              memory: 256Mi
            limits:
              cpu: 500m
              memory: 512Mi
          volumeMounts:
            - name: bare-repo
              mountPath: /bare-repo
            - name: nemo-config
              mountPath: /etc/nemo
              readOnly: true
          startupProbe:
            httpGet:
              path: /health
              port: 8080
            failureThreshold: 30
            periodSeconds: 2
            timeoutSeconds: 3
          livenessProbe:
            tcpSocket:
              port: 8080
            periodSeconds: 15
            timeoutSeconds: 3
          readinessProbe:
            httpGet:
              path: /health
              port: 8080
            periodSeconds: 10
            timeoutSeconds: 3
      volumes:
        - name: bare-repo
          persistentVolumeClaim:
            claimName: nemo-bare-repo
        - name: nemo-config
          configMap:
            name: nemo-config
---
apiVersion: v1
kind: Service
metadata:
  name: nemo-api-server
  namespace: nemo-system
spec:
  selector:
    app: nemo-api-server
  ports:
    - port: 8080
      targetPort: 8080
YAML

  loop_engine_yaml = <<-YAML
apiVersion: apps/v1
kind: Deployment
metadata:
  name: nemo-loop-engine
  namespace: nemo-system
  labels:
    app: nemo-loop-engine
spec:
  replicas: 1
  selector:
    matchLabels:
      app: nemo-loop-engine
  template:
    metadata:
      labels:
        app: nemo-loop-engine
      annotations:
        config-checksum: "${local.config_checksum}"
    spec:
      serviceAccountName: nemo-loop-engine
      ${local.image_pull_secrets_snippet}
      securityContext:
        fsGroup: 1000
      containers:
        - name: loop-engine
          image: ${var.control_plane_image}
          args: ["loop-engine"]
          env:
            - name: DATABASE_URL
              valueFrom:
                secretKeyRef:
                  name: nemo-postgres-credentials
                  key: DATABASE_URL
            - name: GIT_HOST_TOKEN
              valueFrom:
                secretKeyRef:
                  name: nemo-git-host-token
                  key: GIT_HOST_TOKEN
            - name: BARE_REPO_PATH
              value: /bare-repo
            - name: AGENT_IMAGE
              value: ${var.agent_base_image}
          volumeMounts:
            - name: bare-repo
              mountPath: /bare-repo
            - name: nemo-config
              mountPath: /etc/nemo
              readOnly: true
          resources:
            requests:
              cpu: 100m
              memory: 256Mi
            limits:
              cpu: 500m
              memory: 512Mi
          livenessProbe:
            exec:
              command: ["kill", "-0", "1"]
            initialDelaySeconds: 15
            periodSeconds: 30
            timeoutSeconds: 3
      volumes:
        - name: bare-repo
          persistentVolumeClaim:
            claimName: nemo-bare-repo
        - name: nemo-config
          configMap:
            name: nemo-config
YAML
}

# --- Nemo config ---

resource "null_resource" "k8s_nemo_config" {
  depends_on = [null_resource.k8s_foundation]

  triggers = {
    config_hash = sha256(local.nemo_config_yaml)
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
      "echo '${base64encode(local.nemo_config_yaml)}' | base64 -d | kubectl apply -f -",
    ]
  }
}

# --- Repo Init Job ---

resource "null_resource" "k8s_repo_init" {
  depends_on = [
    null_resource.k8s_foundation,
    null_resource.k8s_secrets,
    null_resource.k8s_config,
    null_resource.k8s_nemo_config,
    null_resource.ssh_keyscan,
  ]

  triggers = {
    manifest_hash = sha256(local.repo_init_yaml)
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
      # Delete previous job if it exists (jobs are immutable)
      "kubectl -n nemo-system delete job nemo-repo-init --ignore-not-found",
      "echo '${base64encode(local.repo_init_yaml)}' | base64 -d | kubectl apply -f -",
      # Wait for completion. The job script already tolerates missing deploy keys
      # (git fetch failure is non-fatal inside the container). A timeout here means
      # a real infrastructure failure (bad PVC, secret mount, etc.) — fail hard.
      "kubectl -n nemo-system wait --for=condition=complete job/nemo-repo-init --timeout=600s",
    ]
  }
}

# --- API Server ---

resource "null_resource" "k8s_api_server" {
  depends_on = [
    null_resource.k8s_postgres,
    null_resource.k8s_repo_init,
    null_resource.k8s_secrets,
    null_resource.k8s_config,
    null_resource.k8s_nemo_config,
    null_resource.k8s_registry_creds,
  ]

  triggers = {
    manifest_hash = sha256(local.api_server_yaml)
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
      "echo '${base64encode(local.api_server_yaml)}' | base64 -d | kubectl apply -f -",
    ]
  }
}

# --- Loop Engine ---

resource "null_resource" "k8s_loop_engine" {
  depends_on = [
    null_resource.k8s_postgres,
    null_resource.k8s_repo_init,
    null_resource.k8s_secrets,
    null_resource.k8s_config,
    null_resource.k8s_nemo_config,
    null_resource.k8s_registry_creds,
  ]

  triggers = {
    manifest_hash = sha256(local.loop_engine_yaml)
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
      "echo '${base64encode(local.loop_engine_yaml)}' | base64 -d | kubectl apply -f -",
    ]
  }
}

# --- Health check: wait for API server to be ready ---

resource "null_resource" "health_check" {
  depends_on = [
    null_resource.k8s_api_server,
    null_resource.k8s_loop_engine,
    null_resource.k8s_networking_tls,
    null_resource.k8s_networking_ip,
  ]

  triggers = {
    api_server_hash  = sha256(local.api_server_yaml)
    loop_engine_hash = sha256(local.loop_engine_yaml)
    server_ip        = var.server_ip
    has_domain       = tostring(local.has_domain)
  }

  connection {
    type        = "ssh"
    host        = var.server_ip
    user        = var.ssh_user
    private_key = var.ssh_private_key
  }

  provisioner "remote-exec" {
    inline = [
      "echo 'Waiting for Nemo API server to be ready...'",
      "kubectl -n nemo-system rollout status deployment/nemo-api-server --timeout=600s",
      "kubectl -n nemo-system rollout status deployment/nemo-loop-engine --timeout=600s",
      "SVC_IP=$(kubectl -n nemo-system get svc nemo-api-server -o jsonpath='{.spec.clusterIP}')",
      "TRIES=0; until curl -sf http://$SVC_IP:8080/health >/dev/null 2>&1 || [ $TRIES -ge 60 ]; do sleep 2; TRIES=$((TRIES+1)); done",
      "curl -sf http://$SVC_IP:8080/health || { echo 'ERROR: API server health check failed after 120s'; exit 1; }",
      "echo 'Nemo is ready.'",
    ]
  }
}
