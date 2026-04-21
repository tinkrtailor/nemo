# Control plane: nemo.toml config, repo-init job, API server, loop engine — applied via SSH+kubectl.

locals {
  # dashboard_secure_cookie resolution:
  # - If operator explicitly set it, use that.
  # - Otherwise, default to false when no TLS-terminating domain is configured
  #   (IP-only / Tailscale HTTP deployments — Secure cookies get dropped over HTTP).
  # - When a domain is set, leave unset (binary auto-detects and defaults to Secure).
  _dashboard_secure_cookie_resolved = (
    var.dashboard_secure_cookie != null ? var.dashboard_secure_cookie :
    ((var.domain == null || var.domain == "") ? false : null)
  )

  nautiloop_toml = <<-TOML
[cluster]
git_repo_url = "${var.git_repo_url}"
agent_image = "${var.agent_base_image}"
sidecar_image = "${var.sidecar_image}"
${var.image_pull_secret_dockerconfigjson != null ? "image_pull_secret = \"nautiloop-registry-creds\"" : ""}
${local._dashboard_secure_cookie_resolved != null ? "dashboard_secure_cookie = ${local._dashboard_secure_cookie_resolved}" : ""}
TOML

  config_checksum = sha256(local.nautiloop_toml)

  nautiloop_config_yaml = <<-YAML
apiVersion: v1
kind: ConfigMap
metadata:
  name: nautiloop-config
  namespace: nautiloop-system
data:
  nemo.toml: |
    ${indent(4, local.nautiloop_toml)}
YAML

  image_pull_secrets_snippet = var.image_pull_secret_dockerconfigjson != null ? "imagePullSecrets:\n        - name: nautiloop-registry-creds" : ""

  repo_init_yaml = <<-YAML
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
                  name: nautiloop-cluster-config
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
            claimName: nautiloop-bare-repo
        - name: ssh-key
          secret:
            secretName: nautiloop-repo-ssh-key
            defaultMode: 0400
        - name: ssh-known-hosts
          configMap:
            name: nautiloop-ssh-known-hosts
      restartPolicy: OnFailure
YAML

  api_server_yaml = <<-YAML
apiVersion: apps/v1
kind: Deployment
metadata:
  name: nautiloop-api-server
  namespace: nautiloop-system
  labels:
    app: nautiloop-api-server
spec:
  replicas: 1
  selector:
    matchLabels:
      app: nautiloop-api-server
  template:
    metadata:
      labels:
        app: nautiloop-api-server
      annotations:
        config-checksum: "${local.config_checksum}"
    spec:
      serviceAccountName: nautiloop-api-server
      ${local.image_pull_secrets_snippet}
      securityContext:
        fsGroup: 1000
      initContainers:
        - name: auth-sidecar
          image: ${var.sidecar_image}
          restartPolicy: Always
          ports:
            - containerPort: 9090
          volumeMounts:
            - name: judge-creds
              mountPath: /secrets/model-credentials
              readOnly: true
          resources:
            requests:
              cpu: 10m
              memory: 20Mi
            limits:
              cpu: 100m
              memory: 64Mi
          startupProbe:
            tcpSocket:
              port: 9090
            periodSeconds: 2
            failureThreshold: 30
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
                  name: nautiloop-postgres-credentials
                  key: DATABASE_URL
            - name: NAUTILOOP_API_KEY
              valueFrom:
                secretKeyRef:
                  name: nautiloop-api-key
                  key: NAUTILOOP_API_KEY
            - name: GIT_HOST_TOKEN
              valueFrom:
                secretKeyRef:
                  name: nautiloop-git-host-token
                  key: GIT_HOST_TOKEN
            - name: BARE_REPO_PATH
              value: /bare-repo
            # The api-server shells out to `git fetch` against the upstream
            # remote (which is configured as git@github.com:owner/repo.git in
            # the bare repo). Without this, ssh has no key, no known_hosts,
            # and falls over with "Host key verification failed". The repo-init
            # job sets these up for the initial clone but the long-running
            # control-plane pods need them too.
            - name: GIT_SSH_COMMAND
              value: "ssh -i /etc/git-ssh/id_ed25519 -o UserKnownHostsFile=/etc/git-ssh-known-hosts/known_hosts -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes"
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
            - name: nautiloop-config
              mountPath: /etc/nautiloop
              readOnly: true
            - name: git-ssh-key
              mountPath: /etc/git-ssh
              readOnly: true
            - name: git-ssh-known-hosts
              mountPath: /etc/git-ssh-known-hosts
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
            claimName: nautiloop-bare-repo
        - name: nautiloop-config
          configMap:
            name: nautiloop-config
        - name: git-ssh-key
          secret:
            secretName: nautiloop-repo-ssh-key
            defaultMode: 0400
        - name: git-ssh-known-hosts
          configMap:
            name: nautiloop-ssh-known-hosts
        - name: judge-creds
          secret:
            secretName: nautiloop-judge-creds
            optional: true
---
apiVersion: v1
kind: Service
metadata:
  name: nautiloop-api-server
  namespace: nautiloop-system
spec:
  selector:
    app: nautiloop-api-server
  ports:
    - port: 8080
      targetPort: 8080
YAML

  loop_engine_yaml = <<-YAML
apiVersion: apps/v1
kind: Deployment
metadata:
  name: nautiloop-loop-engine
  namespace: nautiloop-system
  labels:
    app: nautiloop-loop-engine
spec:
  replicas: 1
  selector:
    matchLabels:
      app: nautiloop-loop-engine
  template:
    metadata:
      labels:
        app: nautiloop-loop-engine
      annotations:
        config-checksum: "${local.config_checksum}"
    spec:
      serviceAccountName: nautiloop-loop-engine
      ${local.image_pull_secrets_snippet}
      securityContext:
        fsGroup: 1000
      initContainers:
        - name: auth-sidecar
          image: ${var.sidecar_image}
          restartPolicy: Always
          ports:
            - containerPort: 9090
          volumeMounts:
            - name: judge-creds
              mountPath: /secrets/model-credentials
              readOnly: true
          resources:
            requests:
              cpu: 10m
              memory: 20Mi
            limits:
              cpu: 100m
              memory: 64Mi
          startupProbe:
            tcpSocket:
              port: 9090
            periodSeconds: 2
            failureThreshold: 30
      containers:
        - name: loop-engine
          image: ${var.control_plane_image}
          args: ["loop-engine"]
          env:
            - name: DATABASE_URL
              valueFrom:
                secretKeyRef:
                  name: nautiloop-postgres-credentials
                  key: DATABASE_URL
            - name: GIT_HOST_TOKEN
              valueFrom:
                secretKeyRef:
                  name: nautiloop-git-host-token
                  key: GIT_HOST_TOKEN
            - name: BARE_REPO_PATH
              value: /bare-repo
            - name: AGENT_IMAGE
              value: ${var.agent_base_image}
            # See api-server above — the loop-engine also reconciles state by
            # invoking `git fetch` against the SSH remote. Without this it
            # crashes the same way.
            - name: GIT_SSH_COMMAND
              value: "ssh -i /etc/git-ssh/id_ed25519 -o UserKnownHostsFile=/etc/git-ssh-known-hosts/known_hosts -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes"
          volumeMounts:
            - name: bare-repo
              mountPath: /bare-repo
            - name: nautiloop-config
              mountPath: /etc/nautiloop
              readOnly: true
            - name: git-ssh-key
              mountPath: /etc/git-ssh
              readOnly: true
            - name: git-ssh-known-hosts
              mountPath: /etc/git-ssh-known-hosts
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
            claimName: nautiloop-bare-repo
        - name: nautiloop-config
          configMap:
            name: nautiloop-config
        - name: git-ssh-key
          secret:
            secretName: nautiloop-repo-ssh-key
            defaultMode: 0400
        - name: git-ssh-known-hosts
          configMap:
            name: nautiloop-ssh-known-hosts
        - name: judge-creds
          secret:
            secretName: nautiloop-judge-creds
            optional: true
YAML
}

# --- Nautiloop config ---

resource "null_resource" "k8s_nautiloop_config" {
  depends_on = [null_resource.k8s_foundation]

  triggers = {
    config_hash = sha256(local.nautiloop_config_yaml)
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
      "echo '${base64encode(local.nautiloop_config_yaml)}' | base64 -d | kubectl apply -f -",
    ]
  }
}

# --- Repo Init Job ---

resource "null_resource" "k8s_repo_init" {
  depends_on = [
    null_resource.k8s_foundation,
    null_resource.k8s_secrets,
    null_resource.k8s_config,
    null_resource.k8s_nautiloop_config,
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
      "kubectl -n nautiloop-system delete job nautiloop-repo-init --ignore-not-found",
      "echo '${base64encode(local.repo_init_yaml)}' | base64 -d | kubectl apply -f -",
      # Wait for completion. The job script already tolerates missing deploy keys
      # (git fetch failure is non-fatal inside the container). A timeout here means
      # a real infrastructure failure (bad PVC, secret mount, etc.) — fail hard.
      "kubectl -n nautiloop-system wait --for=condition=complete job/nautiloop-repo-init --timeout=600s",
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
    null_resource.k8s_nautiloop_config,
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
    null_resource.k8s_nautiloop_config,
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
      "echo 'Waiting for Nautiloop API server to be ready...'",
      "kubectl -n nautiloop-system rollout status deployment/nautiloop-api-server --timeout=600s",
      "kubectl -n nautiloop-system rollout status deployment/nautiloop-loop-engine --timeout=600s",
      "SVC_IP=$(kubectl -n nautiloop-system get svc nautiloop-api-server -o jsonpath='{.spec.clusterIP}')",
      "TRIES=0; until curl -sf http://$SVC_IP:8080/health >/dev/null 2>&1 || [ $TRIES -ge 60 ]; do sleep 2; TRIES=$((TRIES+1)); done",
      "curl -sf http://$SVC_IP:8080/health || { echo 'ERROR: API server health check failed after 120s'; exit 1; }",
      "echo 'Nautiloop is ready.'",
    ]
  }
}
