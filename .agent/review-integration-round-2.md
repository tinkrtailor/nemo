Reviewing the merged Rust workspace end-to-end: file inventory, current branch state, and a full test/clippy pass to catch integration breakage before I inspect source-level issues.
Build, tests, and clippy are clean. Now doing a source-level integration pass over the merged Rust modules, focusing on cross-module contracts, state transitions, API/CLI alignment, and config/job wiring.
The static pass found four non-trivial cross-lane issues. I’m verifying the cited contracts directly in source before I close the review.
1. High: frozen base branch is stored but not actually used after loop creation.
`control-plane/src/api/handlers.rs:130`, `control-plane/src/loop_engine/driver.rs:403`, `control-plane/src/loop_engine/driver.rs:513`, `control-plane/src/loop_engine/driver.rs:737`, `control-plane/src/git/mod.rs:501`
`resolved_default_branch` is persisted on loop creation, but later harden merge, test diffing, ship merge, and PR creation still use live `self.config.cluster.default_branch` or repo-default PR base. If the repo/default branch changes mid-loop, or a loop was started against a non-default base, later stages can diff/merge/target the wrong branch.

2. High: reviewer model override is ignored in the harden audit path.
`cli/src/commands/start.rs:33`, `control-plane/src/api/handlers.rs:122`, `control-plane/src/loop_engine/driver.rs:1204`, `control-plane/src/loop_engine/driver.rs:1254`
`start` sends `model_overrides.reviewer`, and the API stores it in `model_reviewer`, but `audit_stage_config()` always uses `self.config.models.reviewer`. Normal review honors the override; audit does not. Result: `--model-review` silently uses the wrong model for harden / harden-only / ship-with-harden flows.

3. Medium: three-layer config merge is not wired into runtime, and its engineer-config schema diverges from the CLI’s config file.
`control-plane/src/main.rs:28`, `control-plane/src/config/mod.rs:1`, `control-plane/src/config/merged.rs:74`, `control-plane/src/config/engineer.rs:19`, `cli/src/config.rs:5`, `cli/src/commands/config.rs:25`
The control plane runtime loads only `NemoConfig`, while `MergedConfig` and the control-plane `EngineerConfig` path are unused. Separately, the CLI writes a flat `~/.nemo/config.toml`, while the control-plane engineer parser expects `[identity]`, `[models]`, `[limits]`. So engineer-level overrides described by the merged-config lane do not actually affect live execution.

4. Medium: TEST-stage affected-service detection uses raw string prefix matching.
`control-plane/src/loop_engine/driver.rs:521`, `control-plane/src/config/mod.rs:30`
Service selection does `changed_file.starts_with(service.path)`. That misclassifies overlapping paths like `cli` vs `client` or `api` vs `api-gateway`, which can run the wrong service test set or skip the right one.

Not clean. Not converged.

Checks run:
- `cargo test --workspace`: passed
- `cargo clippy --workspace -- -D warnings`: passed

Residual gaps:
- No end-to-end coverage for non-default/frozen base branch behavior.
- No test proving reviewer override affects audit.
- No runtime test covering merged engineer/repo/cluster config application.
- No test for overlapping service path prefixes.
