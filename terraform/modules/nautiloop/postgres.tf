# PostgreSQL: deployment, service, daily backup cronjob — applied via SSH+kubectl.

locals {
  postgres_yaml = <<-YAML
apiVersion: apps/v1
kind: Deployment
metadata:
  name: nemo-postgres
  namespace: nemo-system
  labels:
    app: nemo-postgres
spec:
  replicas: 1
  selector:
    matchLabels:
      app: nemo-postgres
  template:
    metadata:
      labels:
        app: nemo-postgres
    spec:
      containers:
        - name: postgres
          image: postgres:16-alpine
          ports:
            - containerPort: 5432
          env:
            - name: POSTGRES_DB
              value: nemo
            - name: POSTGRES_USER
              value: nemo
            - name: POSTGRES_PASSWORD
              valueFrom:
                secretKeyRef:
                  name: nemo-postgres-credentials
                  key: password
            - name: PGDATA
              value: /var/lib/postgresql/data/pgdata
          volumeMounts:
            - name: postgres-data
              mountPath: /var/lib/postgresql/data
          resources:
            requests:
              cpu: 250m
              memory: 512Mi
            limits:
              cpu: "1"
              memory: 2Gi
          livenessProbe:
            exec:
              command: ["pg_isready", "-U", "nemo"]
            initialDelaySeconds: 15
            periodSeconds: 10
          readinessProbe:
            exec:
              command: ["pg_isready", "-U", "nemo"]
            initialDelaySeconds: 5
            periodSeconds: 5
      volumes:
        - name: postgres-data
          persistentVolumeClaim:
            claimName: nemo-postgres-data
---
apiVersion: v1
kind: Service
metadata:
  name: nemo-postgres
  namespace: nemo-system
spec:
  selector:
    app: nemo-postgres
  ports:
    - port: 5432
      targetPort: 5432
---
apiVersion: batch/v1
kind: CronJob
metadata:
  name: nemo-postgres-backup
  namespace: nemo-system
spec:
  schedule: "0 2 * * *"
  jobTemplate:
    spec:
      template:
        spec:
          containers:
            - name: backup
              image: postgres:16-alpine
              command: ["/bin/sh", "-c"]
              args:
                - |
                  set -e
                  find /data/backups -name "nemo-*.sql.gz" -mtime +7 -delete 2>/dev/null || true
                  PGPASSWORD="$POSTGRES_PASSWORD" pg_dump -h nemo-postgres -U nemo nemo | \
                    gzip > /data/backups/nemo-$(date +%Y%m%d-%H%M%S).sql.gz
                  echo "Backup completed successfully"
              env:
                - name: POSTGRES_PASSWORD
                  valueFrom:
                    secretKeyRef:
                      name: nemo-postgres-credentials
                      key: password
              volumeMounts:
                - name: backup-volume
                  mountPath: /data/backups
          volumes:
            - name: backup-volume
              hostPath:
                path: /data/backups
                type: DirectoryOrCreate
          restartPolicy: OnFailure
YAML
}

resource "null_resource" "k8s_postgres" {
  depends_on = [
    null_resource.k8s_foundation,
    null_resource.k8s_secrets,
  ]

  triggers = {
    manifest_hash = sha256(local.postgres_yaml)
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
      "echo '${base64encode(local.postgres_yaml)}' | base64 -d | kubectl apply -f -",
      "kubectl -n nemo-system rollout status deployment/nemo-postgres --timeout=300s",
    ]
  }
}
