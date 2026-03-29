1. `images/base/nemo-agent-entry:111`, `images/base/nemo-agent-entry:122`, `control-plane/src/types/verdict.rs:56` — Severity: critical  
   Implement/revise/review/audit `exec` the model CLIs directly in `stream-json`/`json` mode, but Lane C’s output contract requires the entrypoint to emit a single `NEMO_RESULT:` line. In production, pod logs will contain CLI event streams, not the required envelope, so the control plane cannot parse stage results.  
   Fix: run the CLI as a child process, extract the final assistant payload, write `/output/result.json`, and print exactly one `NEMO_RESULT:` line.

2. `control-plane/src/k8s/job_builder.rs:481`, `control-plane/src/k8s/job_builder.rs:485`, `control-plane/src/k8s/job_builder.rs:487` — Severity: critical  
   The “egress enforcement” init container does not actually enforce proxy-only egress. It explicitly allows all TCP from UID 1000 and never adds a default DROP for other outbound TCP, so the agent can bypass the sidecar and connect directly to the internet.  
   Fix: default-drop outbound traffic for the agent UID and only allow loopback / explicit sidecar ports, or transparently redirect outbound traffic through the proxy.

3. `control-plane/src/k8s/job_builder.rs:383`, `control-plane/src/k8s/job_builder.rs:439` — Severity: critical  
   Claude session credentials are mounted directly into the untrusted agent container at `/work/home/.claude`. Any adversarial code running in IMPLEMENT/REVISE can read and exfiltrate the engineer’s auth material.  
   Fix: move Claude auth behind the sidecar or another broker; do not mount raw model credentials into the agent container.

4. `control-plane/src/k8s/job_builder.rs:307`, `terraform/k8s.tf:29` — Severity: critical  
   Agent jobs mount PVC `nemo-bare-repo` in namespace `nemo-jobs`, but Terraform creates that PVC only in `nemo-system`. Namespaced PVCs are not cross-namespace mountable, so job pods will fail to start.  
   Fix: create the PVC in `nemo-jobs` too, or redesign shared storage so the job namespace can mount it legally.

5. `control-plane/src/k8s/job_builder.rs:307`, `control-plane/src/k8s/job_builder.rs:403` — Severity: critical  
   The job mounts the entire bare-repo PVC at `/work` with no `subPath`, so the agent gets the PVC root instead of the prepared per-loop worktree. That breaks the worktree model and risks corrupting the shared repo area.  
   Fix: pass the concrete worktree path into the job builder and mount it via `VolumeMount.sub_path`.

6. `images/sidecar/main.go:504`, `images/sidecar/main.go:415`, `images/sidecar/main.go:467`, `control-plane/src/k8s/job_builder.rs:134` — Severity: high  
   The git proxy only restricts by host, not by repository path. A malicious agent can ask the localhost SSH proxy to run `git-upload-pack`/`git-receive-pack` against any repo on the same host that the mounted SSH key can access.  
   Fix: parse the configured `GIT_REPO_URL` fully and reject any SSH exec whose repo path does not exactly match it.

7. `images/sidecar/main.go:458`, `terraform/k8s.tf:115`, `control-plane/src/k8s/job_builder.rs:449` — Severity: high  
   The sidecar connects to the real git remote with `ssh.InsecureIgnoreHostKey()`. Terraform creates `nemo-ssh-known-hosts`, but the sidecar never mounts or uses it. This leaves git fetch/push vulnerable to MITM.  
   Fix: mount known-hosts into the sidecar and use a strict host key callback such as `knownhosts.New(...)`.

8. `terraform/control-plane.tf:60`, `terraform/control-plane.tf:190` — Severity: critical  
   `DATABASE_URL` uses `$(POSTGRES_PASSWORD)` inside a literal env value. Kubernetes does not shell-expand env var references in `value`, so both deployments get an unusable DSN string and fail DB connection.  
   Fix: store the full DSN in a Secret, or pass discrete DB env vars and assemble the URL in application code.

9. `terraform/control-plane.tf:313`, `terraform/k8s.tf:61`, `terraform/variables.tf:1` — Severity: high  
   `repo_init` mounts secret `nemo-repo-ssh-key`, but this Terraform module never creates it and exposes no input for it. Fresh-cluster bootstrap against an SSH repo will fail immediately.  
   Fix: add a variable/resource for the bootstrap SSH key, or support HTTPS bootstrap with a token.

10. `images/base/nemo-agent-entry:13`, `images/base/Dockerfile:16` — Severity: high  
    The sidecar readiness loop depends on `bc`, but the base image never installs it. The agent entrypoint can fail before any stage runs.  
    Fix: install `bc`, or rewrite the timeout loop with integer shell arithmetic / `date +%s`.

11. `images/base/nemo-agent-entry:162`, `images/base/nemo-agent-entry:173`, `images/base/nemo-agent-entry:176` — Severity: medium  
    TEST stage merges stderr into stdout, then always records `stderr: ""`, and maps every non-zero exit to `failed`. Harness failures like `command not found`, timeouts, or infra errors can never surface as `ci_status: "unknown"`.  
    Fix: capture stdout/stderr separately and map infrastructure failures to `unknown`.

12. `terraform/k8s.tf:128`, `terraform/control-plane.tf:241` — Severity: high  
    The `ssh-keyscan` fallback updates the ConfigMap after creation, but `repo_init` depends only on the original ConfigMap resource, not the fallback patch step. First apply can race and run `git fetch` with empty `known_hosts`.  
    Fix: make `repo_init` depend on the fallback resource when `ssh_known_hosts` is empty, or generate the final ConfigMap content in one Terraform step.

13. `terraform/main.tf:33`, `terraform/main.tf:62` — Severity: medium  
    Terraform `file("~/.ssh/id_ed25519")` does not expand `~`, so SSH-based provisioning breaks on a normal machine unless the path is manually expanded.  
    Fix: use `pathexpand("~/.ssh/id_ed25519")` or make the key path an input variable.
