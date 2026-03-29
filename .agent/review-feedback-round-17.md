Inspecting current Lane C diff and recent commits to review only the changes in scope.
Worktree is clean aside from an untracked local sandbox file. Reviewing the branch delta from `main` and the latest fix commit for any remaining issues.
Found one likely regression in the round-16 Terraform fix. Verifying how the jobs namespace consumes `nemo-ssh-known-hosts` before finalizing.
1. High: the new `nemo-jobs` copy of `nemo-ssh-known-hosts` is never populated by the `ssh-keyscan` fallback, so the exact case this fix was meant to cover still breaks agent pods when `var.ssh_known_hosts` is empty. `terraform/k8s.tf:200` creates the jobs-namespace ConfigMap from `var.ssh_known_hosts`, but `terraform/k8s.tf:213` only runs `kubectl ... -n nemo-system apply -f -`, leaving `nemo-jobs/nemo-ssh-known-hosts` empty. Since job pods mount that ConfigMap for sidecar host-key verification (`control-plane/src/k8s/job_builder.rs:362`), SSH git access from agent jobs still fails in the fallback path.

Not converged.

Assumptions:
- `.claude/.sandbox-claude.json` is a local untracked file and out of scope for review.
