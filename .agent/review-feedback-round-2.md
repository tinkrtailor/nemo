Not clean — not converged.

I reviewed all source under `control-plane/src`, `cli/src`, `images`, `terraform`, and `.nemo/prompts`.

**Round 1**
- Fixed: proxy egress drop + loopback-only agent access, per-worktree `subPath` mount, git repo-path restriction, Terraform `DATABASE_URL` secret wiring, repo SSH key input, `bc` removal, `ssh-keyscan` dependency, `pathexpand(...)`.
- Still broken — Critical — output contract is not wired end-to-end: prompts still tell agents to emit `NEMO_RESULT:`, the entrypoint wraps that again, and the control plane still reads `.agent/*-verdict.json` from git instead of pod logs. See `images/base/nemo-agent-entry:140`, `.nemo/prompts/review.md:69`, `.nemo/prompts/spec-audit.md:62`, `control-plane/src/loop_engine/driver.rs:260`, `control-plane/src/types/verdict.rs:34`.
- Still broken — Critical — Claude-backed stages have no working auth path now: `implement`/`revise` still call `claude`, but no Claude secret is mounted and no sidecar broker exists. See `images/base/nemo-agent-entry:157`, `control-plane/src/k8s/job_builder.rs:342`, `control-plane/src/k8s/job_builder.rs:383`.
- Still broken — Critical — the bare-repo PVC “fix” created two different namespaced claims, so the loop engine and job pods do not share the same storage. See `terraform/k8s.tf:25`, `terraform/k8s.tf:46`, `terraform/control-plane.tf:140`, `control-plane/src/k8s/job_builder.rs:304`.
- Still broken — High — SSH host verification still falls back to `ssh.InsecureIgnoreHostKey()` when `known_hosts` is absent/unreadable, so MITM remains possible. See `images/sidecar/main.go:474`.
- Still broken — Medium — test harness failures still do not map correctly to `unknown`; e.g. timeout exit `124` is reported as `failed`. See `images/base/nemo-agent-entry:227`.

**New Issues**
- Critical — jobs mount `nemo-creds-{engineer}` secrets, but `nemo auth` only writes credentials to Postgres; no K8s secret is ever created, so pods will fail to mount creds. See `control-plane/src/api/handlers.rs:394`, `control-plane/src/state/postgres.rs:545`, `control-plane/src/k8s/job_builder.rs:346`.
- Critical — secrets are stored plaintext in Postgres and then injected into agent env as `NEMO_CRED_*`, so untrusted agent code can exfiltrate them directly. See `control-plane/src/api/handlers.rs:392`, `control-plane/src/state/postgres.rs:548`, `control-plane/src/loop_engine/driver.rs:1144`, `control-plane/src/k8s/job_builder.rs:275`.
- Critical — the control plane never actually creates the persistent per-loop worktree it later mounts; it only derives a string path. Jobs can start on a nonexistent subpath. See `control-plane/src/loop_engine/driver.rs:1159`, `control-plane/src/k8s/job_builder.rs:391`, `control-plane/src/git/mod.rs:235`.
- High — the egress proxy will CONNECT/dial arbitrary private or cluster-internal addresses, so the agent can pivot through the sidecar into internal services. See `images/sidecar/main.go:179`, `images/sidecar/main.go:233`.
- High — sidecar readiness can go green even if a listener never bound; the startup loop never fails on per-port timeout. See `images/sidecar/main.go:662`, `images/sidecar/main.go:673`.
- High — kubeconfig is fetched with `StrictHostKeyChecking=no`, allowing SSH MITM to hand back a malicious kubeconfig. See `terraform/main.tf:67`.
- High — `nemo auth` is still mismatched to Lane C: wrong Claude/OpenAI credential paths and no `--ssh` support, so required runtime creds cannot be provisioned reliably. See `cli/src/commands/auth.rs:27`, `cli/src/commands/auth.rs:31`, `cli/src/main.rs:135`.
- High — a transient `gh pr view` failure is treated as “no PR”, and branch creation may delete/reset an active remote branch. See `control-plane/src/git/mod.rs:176`, `control-plane/src/git/mod.rs:200`, `control-plane/src/git/mod.rs:263`.

If you want, I can do a Round 3 pass after fixes and keep the same adversarial bar.
