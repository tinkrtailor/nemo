BUGS FOUND

- High — `control-plane/src/loop_engine/driver.rs:1248`
  - The loop derives `worktree_path` as `worktrees/<branch-safe>` and passes that through to both git setup and pod mounting.
  - But `ensure_worktree` resolves that relative to the bare repo root, so the actual checkout is created at `bare_repo_path/worktrees/...` in `control-plane/src/git/mod.rs:542`.
  - The Kubernetes job mount uses that same string as a PVC `subPath` at `control-plane/src/k8s/job_builder.rs:377`, which mounts from the PVC root, not from inside `bare_repo.git/`.
  - With the default bare repo path in `control-plane/src/main.rs:60` set to `/data/bare-repo.git`, the real worktree lands under `/data/bare-repo.git/worktrees/...` while the pod tries to mount `worktrees/...`.
  - Result: stage pods mount the wrong directory and can fail or run against an empty/non-worktree path.

I read all 35 Rust production source files under `control-plane/src` and `cli/src`. No other real production bugs stood out at high confidence.
