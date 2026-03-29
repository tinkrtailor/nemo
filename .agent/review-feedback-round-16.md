Checking the current branch state and diff for Lane C, then I’ll report only review findings.
I found one untracked local file, `.claude/.sandbox-claude.json`; I’m ignoring it and reviewing the branch diff against `main`.
I found three candidate issues from the branch review. I’m verifying the exact code paths and line references before I finalize.
Not converged.

1. High - `control-plane/src/k8s/job_builder.rs:171`, `control-plane/src/k8s/job_builder.rs:362-367`, `terraform/k8s.tf:187-196`
Agent Jobs are created in `cfg.namespace` (`nemo-jobs`), but the `ssh-known-hosts` ConfigMap is only provisioned in `nemo-system`. ConfigMaps are namespace-scoped, so pods mounting `ssh-known-hosts` in `nemo-jobs` will fail with `FailedMount` before either container starts.

2. High - `control-plane/src/loop_engine/driver.rs:505-512`
The TEST transition no longer computes affected services from `git diff`; it serializes every configured service from `self.config.services`. That contradicts `specs/lane-c-agent-runtime.md:165` / Lane C FR-42a, and it means unrelated failing services can block valid changes while also making test rounds much slower.

3. Medium - `images/base/nemo-agent-entry:140-150`, `.nemo/prompts/implement.md:33-35`, `.nemo/prompts/spec-revise.md:29-31`, `control-plane/src/loop_engine/driver.rs:303-307`, `control-plane/src/loop_engine/driver.rs:446-463`
The control plane now expects typed `NEMO_RESULT.data` for implement/revise (`session_id`, `revised_spec_path`, etc.), but the prompts tell Claude to “output nothing special” and the entrypoint just wraps the last assistant message blob as `data`. In practice those fields will usually be missing, so session resume is not persisted and revise-stage spec path changes are not reliably detected.

Assumption: I ignored the untracked local file `.claude/.sandbox-claude.json` since this was a read-only review.
l-plane/src/api/handlers.rs:301`, `control-plane/src/api/handlers.rs:332`, `cli/src/commands/approve.rs:17`, `cli/src/commands/resume.rs:17`
The CLI has a friendly “not applicable” success path for `approve`/`resume`, but the API never returns that shape on invalid state; it returns a conflict error instead. The fallback branch in the CLI is dead code.

I read every Rust source file under `control-plane/src` and `cli/src`.
