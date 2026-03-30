Starting with a full Rust source inventory and a build-level sanity check so I can review cross-lane integration issues against the actual merged code, not assumptions.
The workspace builds, so I’m switching from compile-level sanity to source-level integration review across every Rust module and the cross-lane boundaries: API/types/state, loop engine/runtime, k8s/terraform-facing config, git, and CLI.
I’ve confirmed one class of issue already: the merged code compiles, but some integration paths still hardcode assumptions that Lane B tried to make configurable. I’m verifying the remaining runtime edges directly in the loop driver and git layer before I finalize findings.
**Findings**
1. High: default-branch handling is still hardcoded to `main`, so merged lanes can branch, diff, and resolve merge SHAs against the wrong base on repos whose default branch is not `main`.
`control-plane/src/api/handlers.rs:49-53`, `control-plane/src/git/mod.rs:216-223`, `control-plane/src/git/mod.rs:561-567`, `control-plane/src/git/mod.rs:594-603`
`/start` reads specs from `origin/main`, branch creation starts from `origin/main`, `changed_files()` diffs against `origin/main`, and `merge_pr()` returns `origin/main` after merge. Lane B added configurable default-branch support in `NemoConfig::default_remote_ref()`, but these runtime paths do not use it.

2. High: the new `resolved_default_branch` schema is not integrated into Rust state types or Postgres queries, so the intended per-loop base-branch freeze is currently inert.
`control-plane/migrations/20260329000002_add_resolved_default_branch.sql:1-8`, `control-plane/src/types/mod.rs:244-277`, `control-plane/src/state/postgres.rs:120-163`, `control-plane/src/state/postgres.rs:205-260`, `control-plane/src/state/postgres.rs:356-388`
The migration adds `loops.resolved_default_branch`, but `LoopRecord` has no such field, `row_to_loop_record()` never reads it, and `create_loop()`/`update_loop()` never write it. That is schema/runtime drift and means PR/merge behavior cannot actually freeze to the branch chosen at submission time.

3. Medium: audit/revise prompt naming is inconsistent across the merged code, and one side points at files that do not exist.
`control-plane/src/types/mod.rs:141-147`, `control-plane/src/loop_engine/driver.rs:1196-1216`, `.nemo/prompts/spec-audit.md:1`, `.nemo/prompts/spec-revise.md:1`
`Stage::prompt_filename()` maps audit/revise to `spec-audit.md` and `spec-revise.md`, and those are the files present in `.nemo/prompts/`. But the loop driver dispatches `.nemo/prompts/audit.md` and `.nemo/prompts/revise.md`, which are absent. That is a cross-lane runtime integration bug if prompt loading uses `StageConfig.prompt_template`.

4. Medium: Lane B’s `BareRepo` implementation is effectively dead code after the merge; production wiring uses a different git path.
`control-plane/src/git/bare_repo.rs:31-243`, `control-plane/src/main.rs:64-66`
The runtime instantiates `git::bare::BareRepoGitOperations` from `git/mod.rs`, while `git/bare_repo.rs` contains a separate `BareRepo` abstraction with its own worktree/divergence logic that is never referenced outside its tests. Any fixes or assumptions in that file are not exercised by the control plane.

5. Low: `job_builder::job_name()` no longer matches actual runtime job naming and is unused.
`control-plane/src/k8s/job_builder.rs:38-45`, `control-plane/src/k8s/job_builder.rs:225-228`
Real jobs include the retry suffix `-t{attempt}`, but the helper omits it. It is dead today, but if reused later it will generate wrong names and break retry/job lookups.

**Checks**
- Read all Rust source under `control-plane/` and `cli/`.
- Ran `cargo check --workspace`: passes.

Not clean. No compile-time type/import breakage showed up, but the merged tree still has real runtime integration issues around base-branch selection, schema wiring, and prompt-file resolution.
