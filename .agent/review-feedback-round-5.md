Not clean. I read all Rust source under `control-plane/src` and `cli/src` and found 6 real production bugs.

- Critical — `control-plane/src/k8s/job_builder.rs:442`: every job pod uses an init container based on `alpine:3.19` but the script calls `iptables`; Alpine does not ship `iptables` by default, so the init container fails and no agent jobs can start.
- High — `control-plane/src/k8s/job_builder.rs:347`: the `ssh-key` secret projection requires key `ssh` unconditionally; if an engineer has only model creds, pod startup fails before the job runs.
- High — `control-plane/src/git/mod.rs:179`: when the branch already exists and PR state is unknown/absent, `create_branch` reuses the old local branch “as-is” instead of recreating from `origin/main`; rerunning the same spec can start from stale commits/artifacts from a previous terminal run.
- High — `control-plane/src/api/handlers.rs:435`: `POST /credentials` with `valid=false` still writes or leaves the Kubernetes Secret in place, and jobs always mount that secret; “invalidated” credentials remain usable by pods.
- High — `cli/src/commands/auth.rs:41`: `nemo auth --claude` only checks `~/.config/claude-code/credentials.json` and `~/.claude/credentials.json`, but this repo’s own runtime expects `~/.claude/.credentials.json` (`claude-worktree.sh:252`); users can log in successfully and still fail to upload Claude auth.
- High — `cli/src/commands/auth.rs:54`: `nemo auth --openai` looks in `~/.config/openai/credentials.json`, but the repo contract is `~/.config/opencode/` for reviewer auth (`specs/lane-a-core-loop.md:562`); on a correctly configured machine, OpenAI reviewer creds are never discovered.

If you want, I can do round 6 with another read-only pass after these are fixed.
